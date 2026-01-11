#!/bin/bash
set -e

# Sync Flow releases to CDN server
# Usage: ./scripts/sync-cdn.sh [version]
# If no version specified, syncs latest release

REPO="nikitavoloboev/flow"
CDN_HOST="root@100.114.156.47"
CDN_PATH="/var/www/cdn.myflow.sh"

# Get version
if [ -n "${1:-}" ]; then
  VERSION="$1"
else
  echo "Fetching latest version..."
  VERSION=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')
fi

if [ -z "$VERSION" ]; then
  echo "Error: Could not determine version"
  exit 1
fi

echo "Syncing version: $VERSION"

TARGETS=(
  "x86_64-apple-darwin"
  "aarch64-apple-darwin"
  "x86_64-unknown-linux-gnu"
  "aarch64-unknown-linux-gnu"
)

# Create temp directory
TMP_DIR=$(mktemp -d)
trap "rm -rf $TMP_DIR" EXIT

# Download all artifacts
echo "Downloading artifacts..."
for target in "${TARGETS[@]}"; do
  url="https://github.com/${REPO}/releases/download/${VERSION}/flow-${target}.tar.gz"
  echo "  Downloading flow-${target}.tar.gz..."
  curl -fsSL -o "$TMP_DIR/flow-${target}.tar.gz" "$url" || echo "  Warning: failed to download $target"
done

# Download checksums
echo "  Downloading checksums.txt..."
curl -fsSL -o "$TMP_DIR/checksums.txt" "https://github.com/${REPO}/releases/download/${VERSION}/checksums.txt" || true

# Create version directory on CDN
echo "Creating directory on CDN..."
ssh "$CDN_HOST" "mkdir -p ${CDN_PATH}/${VERSION}"

# Upload files
echo "Uploading to CDN..."
scp "$TMP_DIR"/* "${CDN_HOST}:${CDN_PATH}/${VERSION}/"

# Update 'latest' symlink
echo "Updating latest symlink..."
ssh "$CDN_HOST" "cd ${CDN_PATH} && rm -f latest && ln -s ${VERSION} latest"

echo ""
echo "Done! Files available at:"
echo "  https://cdn.myflow.sh/${VERSION}/"
echo "  https://cdn.myflow.sh/latest/"
