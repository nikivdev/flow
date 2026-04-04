#!/usr/bin/env bash

set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This task only supports macOS." >&2
  exit 1
fi

repository_root="${ZED_REPO:-$HOME/repos/zed-industries/zed}"
metadata_path="${ZED_STAGED_METADATA_PATH:-${repository_root}/target/zed-local-deploy/latest.json}"
install_path="${ZED_INSTALL_PATH:-/Applications/Zed.app}"
installed_binary_path="${install_path}/Contents/MacOS/zed"

if [[ ! -d "${repository_root}" ]]; then
  echo "Missing Zed repo at ${repository_root}" >&2
  exit 1
fi

if [[ ! -f "${metadata_path}" ]]; then
  echo "Missing staged build metadata at ${metadata_path}" >&2
  echo "Run \`cd ${repository_root} && f deploy\` first." >&2
  exit 1
fi

metadata_fields=()
while IFS= read -r line; do
  metadata_fields+=("${line}")
done < <(
  python3 - "${metadata_path}" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    metadata = json.load(handle)

print(metadata.get("app_bundle_path", ""))
print(metadata.get("commit_sha", ""))
print(metadata.get("built_at_utc", ""))
PY
)

bundle_path="${metadata_fields[0]:-}"
commit_sha="${metadata_fields[1]:-}"
built_at_utc="${metadata_fields[2]:-}"

if [[ -z "${bundle_path}" ]]; then
  echo "Staged build metadata at ${metadata_path} does not include app_bundle_path" >&2
  exit 1
fi

if [[ ! -d "${bundle_path}" ]]; then
  echo "Expected staged app at ${bundle_path}" >&2
  echo "Run \`cd ${repository_root} && f deploy\` again to refresh the staged build." >&2
  exit 1
fi

if pgrep -f "${installed_binary_path}" >/dev/null 2>&1; then
  echo "Zed is still running from ${installed_binary_path}." >&2
  echo "Quit Zed manually when you are ready, then rerun this task." >&2
  exit 1
fi

rm -rf "${install_path}"
/usr/bin/ditto "${bundle_path}" "${install_path}"

echo "Installed ${install_path}"
echo "Source bundle: ${bundle_path}"
if [[ -n "${commit_sha}" ]]; then
  echo "Built from commit: ${commit_sha}"
fi
if [[ -n "${built_at_utc}" ]]; then
  echo "Built at (UTC): ${built_at_utc}"
fi
