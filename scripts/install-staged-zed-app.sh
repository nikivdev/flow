#!/usr/bin/env bash

set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This task only supports macOS." >&2
  exit 1
fi

repository_root="${ZED_REPO:-$HOME/repos/zed-industries/zed}"
metadata_path="${ZED_STAGED_METADATA_PATH:-${repository_root}/target/zed-local-deploy/latest.json}"
requested_install_path="${ZED_INSTALL_PATH:-}"

wait_for_app_exit() {
  local binary_path=$1
  local timeout_seconds=${2:-120}
  local waited=0

  while pgrep -f "${binary_path}" >/dev/null 2>&1; do
    if (( waited >= timeout_seconds )); then
      return 1
    fi
    sleep 1
    waited=$((waited + 1))
  done

  return 0
}

write_restore_marker() {
  local marker_path=$1

  mkdir -p "$(dirname "${marker_path}")"
  : > "${marker_path}"
  echo "Wrote deploy restore marker: ${marker_path}"
}

launch_post_install_codex_replay() {
  local snapshot_path=$1
  local profile_name=$2
  local minimum_generated_at_ms=$3
  local replay_script="${repository_root}/script/replay-codex-sessions-from-snapshot.py"

  if [[ -z "${snapshot_path}" ]]; then
    return 0
  fi

  if [[ ! -f "${snapshot_path}" ]]; then
    echo "Skipping Codex replay; missing snapshot manifest ${snapshot_path}" >&2
    return 0
  fi

  if [[ ! -f "${replay_script}" ]]; then
    echo "Skipping Codex replay; missing helper ${replay_script}" >&2
    return 0
  fi

  local log_dir="${HOME}/Library/Caches/zed/deploy-replay"
  mkdir -p "${log_dir}"
  local log_path="${log_dir}/$(date +%Y%m%d-%H%M%S)-codex-replay.log"
  local settle_seconds="${ZED_DEPLOY_CODEX_REPLAY_SETTLE_SECONDS:-8}"
  (
    python3 "${replay_script}" \
      --snapshot "${snapshot_path}" \
      --profile "${profile_name}" \
      --settle-seconds "${settle_seconds}" \
      --minimum-generated-at-ms "${minimum_generated_at_ms}"
  ) >"${log_path}" 2>&1 &
  echo "Queued Codex session replay from snapshot: ${snapshot_path}"
  echo "Replay log: ${log_path}"
}

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
print(metadata.get("app_display_name", ""))
print(metadata.get("profile_name", ""))
print(metadata.get("restore_marker_path", ""))
print(metadata.get("pre_deploy_snapshot_path", ""))
PY
)

bundle_path="${metadata_fields[0]:-}"
commit_sha="${metadata_fields[1]:-}"
built_at_utc="${metadata_fields[2]:-}"
app_display_name="${metadata_fields[3]:-}"
profile_name="${metadata_fields[4]:-}"
restore_marker_path="${metadata_fields[5]:-}"
pre_deploy_snapshot_path="${metadata_fields[6]:-}"

if [[ -z "${bundle_path}" ]]; then
  echo "Staged build metadata at ${metadata_path} does not include app_bundle_path" >&2
  exit 1
fi

if [[ ! -d "${bundle_path}" ]]; then
  echo "Expected staged app at ${bundle_path}" >&2
  echo "Run \`cd ${repository_root} && f deploy\` again to refresh the staged build." >&2
  exit 1
fi

bundle_name="$(basename "${bundle_path}")"
install_path="${requested_install_path:-/Applications/${bundle_name}}"
installed_binary_path="${install_path}/Contents/MacOS/zed"
app_display_name="${app_display_name:-${bundle_name%.app}}"
profile_name="${profile_name:-${ZED_PROFILE_NAME:-ZedNikiv}}"
restore_marker_path="${restore_marker_path:-${HOME}/.config/zed/.deploy-restore-last-session}"
preflight_script="${repository_root}/script/zed-deploy-preflight"
allow_risky_codex=false

case "${ZED_INSTALL_ALLOW_RISKY_CODEX:-${ZED_DEPLOY_ALLOW_RISKY_CODEX:-0}}" in
  1|true|TRUE|yes|YES)
    allow_risky_codex=true
    ;;
esac

was_running=false
if pgrep -f "${installed_binary_path}" >/dev/null 2>&1; then
  was_running=true
fi

if [[ "${was_running}" == true ]]; then
  if [[ -x "${preflight_script}" ]]; then
    preflight_args=(
      "--profile" "${profile_name}"
      "--bundle-name" "${app_display_name}"
    )
    if [[ "${allow_risky_codex}" == true ]]; then
      preflight_args+=("--allow-risky-codex")
    fi
    "${preflight_script}" "${preflight_args[@]}"
  else
    echo "Warning: missing preflight script at ${preflight_script}; continuing without a live-session safety check." >&2
  fi

  echo "Requesting ${app_display_name} to quit before install"
  osascript -e "tell application \"${app_display_name}\" to quit" >/dev/null 2>&1 || true

  if ! wait_for_app_exit "${installed_binary_path}" 300; then
    echo "${app_display_name} is still running; aborting install." >&2
    echo "Finish or cancel any pending quit prompts in the app, then rerun this task." >&2
    exit 1
  fi
fi

rm -rf "${install_path}"
/usr/bin/ditto "${bundle_path}" "${install_path}"

if [[ "${was_running}" == true ]]; then
  write_restore_marker "${restore_marker_path}"
  reopen_requested_at_unix_ms=$(python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
)
  echo "Reopening ${install_path}"
  open -na "${install_path}"
  launch_post_install_codex_replay "${pre_deploy_snapshot_path}" "${profile_name}" "${reopen_requested_at_unix_ms}"
fi

echo "Installed ${install_path}"
echo "Source bundle: ${bundle_path}"
if [[ -n "${commit_sha}" ]]; then
  echo "Built from commit: ${commit_sha}"
fi
if [[ -n "${built_at_utc}" ]]; then
  echo "Built at (UTC): ${built_at_utc}"
fi
if [[ "${was_running}" == true ]]; then
  echo "Reopened ${install_path} and requested last-session restore"
else
  echo "Open ${install_path} when you are ready to use the staged build"
fi
