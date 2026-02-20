#!/usr/bin/env bash
set -euo pipefail

REPO="${FLOW_CI_REPO:-nikivdev/flow}"
HOST_TARGET="${1:-}"
FORCE_REINSTALL="${FLOW_CI_FORCE_REINSTALL:-0}"
WAIT_SECS="${FLOW_CI_WAIT_SECS:-120}"

if [[ "${HOST_TARGET}" == "-h" || "${HOST_TARGET}" == "--help" ]]; then
  cat <<'EOF'
Usage: f ci-host-setup [user@ip]

One-command setup for Flow CI host mode:
  1) Optionally set infra host (if user@ip is provided)
  2) Install/register ci-1focus self-hosted GitHub runner
  3) Switch workflows to host runner mode (commit + push)
  4) Print final runner status

Env toggles:
  FLOW_CI_FORCE_REINSTALL=1   Force reinstall even if runner is healthy
  FLOW_CI_WAIT_SECS=180       Wait timeout for GitHub online status (default 120)
EOF
  exit 0
fi

if ! command -v gh >/dev/null 2>&1; then
  echo "gh CLI is required (install GitHub CLI first)." >&2
  exit 1
fi

if ! command -v infra >/dev/null 2>&1; then
  echo "infra CLI is required (install infra first)." >&2
  exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is required." >&2
  exit 1
fi

if [[ -n "${HOST_TARGET}" ]]; then
  echo "Configuring infra host: ${HOST_TARGET}"
  infra host set "${HOST_TARGET}"
else
  if ! infra host show >/dev/null 2>&1; then
    echo "No infra host configured. Run: f ci-host-setup <user@ip>" >&2
    exit 1
  fi
fi

echo "Checking GitHub auth..."
gh auth status >/dev/null

if python3 ./scripts/ci_host_runner.py health --repo "${REPO}" >/dev/null 2>&1 && [[ "${FORCE_REINSTALL}" != "1" ]]; then
  echo "Runner already healthy; skipping reinstall. Set FLOW_CI_FORCE_REINSTALL=1 to force."
else
  echo "Installing/registering ci-1focus runner..."
  attempts=0
  max_attempts=2
  until python3 ./scripts/ci_host_runner.py install --repo "${REPO}"; do
    attempts=$((attempts + 1))
    if [[ $attempts -ge $max_attempts ]]; then
      echo "Runner installation failed after ${max_attempts} attempts." >&2
      exit 1
    fi
    echo "Retrying runner installation (${attempts}/${max_attempts})..."
    sleep 3
  done
fi

echo "Waiting for runner to report online..."
python3 ./scripts/ci_host_runner.py wait-online --repo "${REPO}" --timeout-secs "${WAIT_SECS}" --interval-secs 5

echo "Switching workflows to host mode (commit + push)..."
python3 ./scripts/ci_blacksmith.py host --commit --push

echo "Final runner health:"
python3 ./scripts/ci_host_runner.py health --repo "${REPO}"

echo "Final runner status:"
python3 ./scripts/ci_host_runner.py status --repo "${REPO}" || true
