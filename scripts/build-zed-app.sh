#!/usr/bin/env bash

set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This task only supports macOS." >&2
  exit 1
fi

repository_root="${ZED_REPO:-$HOME/repos/zed-industries/zed}"
release_channel_file="${repository_root}/crates/zed/RELEASE_CHANNEL"
install_path="${ZED_INSTALL_PATH:-/Applications/Zed.app}"

if [[ ! -d "${repository_root}" ]]; then
  echo "Missing Zed repo at ${repository_root}" >&2
  exit 1
fi

if [[ ! -f "${release_channel_file}" ]]; then
  echo "Missing release channel file at ${release_channel_file}" >&2
  exit 1
fi

original_release_channel="$(<"${release_channel_file}")"

restore_release_channel() {
  printf '%s' "${original_release_channel}" > "${release_channel_file}"
}

trap restore_release_channel EXIT

printf '%s' "stable" > "${release_channel_file}"

host_target_triple="$(
  rustc --version --verbose | awk '/^host: / { print $2 }'
)"

case "${host_target_triple}" in
  aarch64-apple-darwin | x86_64-apple-darwin)
    ;;
  *)
    echo "Unsupported macOS target triple: ${host_target_triple}" >&2
    exit 1
    ;;
esac

(
  cd "${repository_root}"
  RELEASE_CHANNEL=stable ZED_RELEASE_CHANNEL=stable ./script/bundle-mac "${host_target_triple}"
)

bundle_path="${repository_root}/target/${host_target_triple}/release/dmg/Zed.app"

if [[ ! -d "${bundle_path}" ]]; then
  echo "Expected bundled app at ${bundle_path}" >&2
  exit 1
fi

rm -rf "${install_path}"
/usr/bin/ditto "${bundle_path}" "${install_path}"

echo "Installed ${install_path}"
