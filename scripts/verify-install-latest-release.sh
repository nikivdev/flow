#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: verify-install-latest-release.sh [options]

Verify that:
  curl -fsSL https://myflow.sh/install.sh | sh

installs the current latest stable Flow release.

Options:
  --tag TAG                 Expected release tag (default: v<Cargo.toml version>)
  --repo OWNER/REPO         GitHub repo to query (default: nikivdev/flow)
  --install-url URL         Installer URL (default: https://myflow.sh/install.sh)
  --latest-timeout SECONDS  Wait up to this many seconds for releases/latest to flip
                            to the expected tag (default: 180)
  --poll-interval SECONDS   Poll interval while waiting for releases/latest (default: 15)
  --skip-asset              Skip the direct release asset verification step
  --keep-temp               Keep temp directories instead of deleting them
  -h, --help                Show this help
EOF
}

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
REPO="nikivdev/flow"
INSTALL_URL="https://myflow.sh/install.sh"
EXPECTED_TAG=""
LATEST_TIMEOUT_SECS=180
POLL_INTERVAL_SECS=15
SKIP_ASSET=0
KEEP_TEMP=0
TMP_HOME=""
TMP_ASSET_DIR=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --tag)
      EXPECTED_TAG="$2"
      shift 2
      ;;
    --repo)
      REPO="$2"
      shift 2
      ;;
    --install-url)
      INSTALL_URL="$2"
      shift 2
      ;;
    --latest-timeout)
      LATEST_TIMEOUT_SECS="$2"
      shift 2
      ;;
    --poll-interval)
      POLL_INTERVAL_SECS="$2"
      shift 2
      ;;
    --skip-asset)
      SKIP_ASSET=1
      shift
      ;;
    --keep-temp)
      KEEP_TEMP=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

cleanup() {
  if [ "$KEEP_TEMP" = "1" ]; then
    return 0
  fi

  if [ -n "$TMP_HOME" ]; then
    rm -rf "$TMP_HOME"
  fi
  if [ -n "$TMP_ASSET_DIR" ]; then
    rm -rf "$TMP_ASSET_DIR"
  fi
}
trap cleanup EXIT

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing required command: $1" >&2
    exit 2
  }
}

normalize_tag() {
  case "$1" in
    v*) printf '%s\n' "$1" ;;
    *) printf 'v%s\n' "$1" ;;
  esac
}

read_cargo_version() {
  python3 - <<'PY' "$ROOT_DIR/Cargo.toml"
import pathlib
import re
import sys

text = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
match = re.search(r'^version\s*=\s*"([^"]+)"', text, re.MULTILINE)
if not match:
    raise SystemExit("failed to read Cargo.toml version")
print(match.group(1))
PY
}

read_latest_tag() {
  curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["tag_name"])'
}

read_binary_version() {
  "$1" --version | python3 -c 'import sys,re; text=sys.stdin.read(); m=re.search(r"flow ([0-9][^ ]*)", text); print(m.group(1) if m else "");'
}

detect_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os-$arch" in
    Darwin-arm64|Darwin-aarch64) printf '%s\n' "aarch64-apple-darwin" ;;
    Darwin-x86_64) printf '%s\n' "x86_64-apple-darwin" ;;
    Linux-x86_64) printf '%s\n' "x86_64-unknown-linux-gnu" ;;
    Linux-arm64|Linux-aarch64) printf '%s\n' "aarch64-unknown-linux-gnu" ;;
    *)
      echo "unsupported platform: $os-$arch" >&2
      exit 2
      ;;
  esac
}

wait_for_expected_latest_tag() {
  local expected_tag="$1"
  local last_seen=""
  local start_ts now_ts

  start_ts="$(date +%s)"
  while :; do
    last_seen="$(read_latest_tag)"
    if [ "$last_seen" = "$expected_tag" ]; then
      printf '%s\n' "$last_seen"
      return 0
    fi

    now_ts="$(date +%s)"
    if [ $((now_ts - start_ts)) -ge "$LATEST_TIMEOUT_SECS" ]; then
      echo "releases/latest still reports ${last_seen} after ${LATEST_TIMEOUT_SECS}s; expected ${expected_tag}" >&2
      return 1
    fi

    sleep "$POLL_INTERVAL_SECS"
  done
}

need_cmd curl
need_cmd python3
need_cmd mktemp
need_cmd tar

if [ -z "$EXPECTED_TAG" ]; then
  EXPECTED_TAG="v$(read_cargo_version)"
else
  EXPECTED_TAG="$(normalize_tag "$EXPECTED_TAG")"
fi

python3 "$ROOT_DIR/scripts/check_release_tag_version.py" "$EXPECTED_TAG" >/dev/null

echo "[verify-install] expected_tag=${EXPECTED_TAG}"
echo "[verify-install] repo=${REPO}"
echo "[verify-install] install_url=${INSTALL_URL}"

LATEST_TAG="$(wait_for_expected_latest_tag "$EXPECTED_TAG")"
echo "[verify-install] latest_tag=${LATEST_TAG}"

TMP_HOME="$(mktemp -d)"
echo "[verify-install] tmp_home=${TMP_HOME}"

HOME="$TMP_HOME" PATH="/usr/bin:/bin:/usr/sbin:/sbin" sh -c \
  'curl -fsSL "$1" | sh' -- "$INSTALL_URL"

INSTALLED_BIN="${TMP_HOME}/.flow/bin/f"
[ -x "$INSTALLED_BIN" ] || {
  echo "installed flow binary missing at ${INSTALLED_BIN}" >&2
  exit 1
}

INSTALLED_VERSION="$(read_binary_version "$INSTALLED_BIN")"
echo "[verify-install] installed_version=${INSTALLED_VERSION}"
if [ "v${INSTALLED_VERSION}" != "$EXPECTED_TAG" ]; then
  echo "fresh temp-home install reported ${INSTALLED_VERSION}, expected ${EXPECTED_TAG#v}" >&2
  exit 1
fi

if [ "$SKIP_ASSET" != "1" ]; then
  TARGET="$(detect_target)"
  TMP_ASSET_DIR="$(mktemp -d)"
  echo "[verify-install] asset_target=${TARGET}"
  echo "[verify-install] tmp_asset_dir=${TMP_ASSET_DIR}"

  curl -fsSLo "${TMP_ASSET_DIR}/flow.tar.gz" \
    "https://github.com/${REPO}/releases/download/${LATEST_TAG}/flow-${TARGET}.tar.gz"

  tar -xzf "${TMP_ASSET_DIR}/flow.tar.gz" -C "${TMP_ASSET_DIR}"
  ASSET_BIN="${TMP_ASSET_DIR}/f"
  [ -x "$ASSET_BIN" ] || {
    echo "release asset did not contain executable f" >&2
    exit 1
  }

  ASSET_VERSION="$(read_binary_version "$ASSET_BIN")"
  echo "[verify-install] asset_version=${ASSET_VERSION}"
  if [ "v${ASSET_VERSION}" != "$EXPECTED_TAG" ]; then
    echo "direct release asset reported ${ASSET_VERSION}, expected ${EXPECTED_TAG#v}" >&2
    exit 1
  fi
fi

echo "[verify-install] OK: installer, latest tag, and direct asset all match ${EXPECTED_TAG}"
