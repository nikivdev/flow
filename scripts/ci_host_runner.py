#!/usr/bin/env python3
"""
Provision and manage a self-hosted GitHub Actions runner on the configured infra Linux host.

This script intentionally uses:
  - `infra host show` for host resolution (no ad-hoc env vars)
  - `gh api` for runner registration/remove tokens

Usage:
  python3 scripts/ci_host_runner.py status
  python3 scripts/ci_host_runner.py install --repo nikivdev/flow
  python3 scripts/ci_host_runner.py remove --repo nikivdev/flow
"""

from __future__ import annotations

import argparse
import json
import re
import shlex
import subprocess
import sys
import time
from dataclasses import dataclass

DEFAULT_REPO = "nikivdev/flow"
DEFAULT_LABELS = "ci-1focus,linux,x64"
DEFAULT_RUNNER_DIR = "/opt/actions-runner"


@dataclass
class HostTriplet:
    user: str
    host: str
    port: str


def run_capture(args: list[str], cwd: str | None = None) -> str:
    result = subprocess.run(
        args,
        cwd=cwd,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return result.stdout.strip()


def run_stream(args: list[str], *, input_text: str | None = None) -> None:
    subprocess.run(args, check=True, text=True, input=input_text)


def load_host_triplet() -> HostTriplet:
    shown = run_capture(["infra", "host", "show"])
    match = re.search(r"Linux\s+host:\s*([^@\s]+)@([^:\s]+):(\d+)", shown)
    if not match:
        raise SystemExit(
            "Unable to parse infra host config. Run: infra host set <user@ip>"
        )
    return HostTriplet(user=match.group(1), host=match.group(2), port=match.group(3))


def gh_api(path: str, *, method: str = "GET", jq: str | None = None) -> str:
    cmd = ["gh", "api"]
    if method != "GET":
        cmd += ["-X", method]
    cmd += [path]
    if jq:
        cmd += ["--jq", jq]
    return run_capture(cmd)


def gh_api_json(path: str, *, method: str = "GET") -> dict:
    out = gh_api(path, method=method)
    return json.loads(out) if out else {}


def ssh_script(host: HostTriplet, script: str) -> None:
    ssh_target = f"{host.user}@{host.host}"
    cmd = [
        "ssh",
        "-p",
        host.port,
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=accept-new",
        ssh_target,
        "bash",
        "-s",
    ]
    run_stream(cmd, input_text=script)


def ssh_capture(host: HostTriplet, script: str) -> str:
    ssh_target = f"{host.user}@{host.host}"
    cmd = [
        "ssh",
        "-p",
        host.port,
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=accept-new",
        ssh_target,
        "bash",
        "-s",
    ]
    result = subprocess.run(
        cmd,
        check=True,
        text=True,
        input=script,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return result.stdout.strip()


def shell_assign(name: str, value: str) -> str:
    return f"{name}={shlex.quote(value)}"


def default_runner_name(host: HostTriplet) -> str:
    safe_host = re.sub(r"[^a-zA-Z0-9-]", "-", host.host)
    return f"ci-1focus-{safe_host}"


def github_runner_state(repo: str, runner_name: str) -> tuple[str, bool | None]:
    payload = gh_api_json(f"repos/{repo}/actions/runners")
    for runner in payload.get("runners", []):
        if runner.get("name") == runner_name:
            return str(runner.get("status", "unknown")), bool(runner.get("busy", False))
    return "missing", None


def host_service_state(host: HostTriplet) -> str:
    script = r'''
set -euo pipefail
state="$(systemctl is-active 'actions.runner.*' 2>/dev/null || true)"
if echo "$state" | grep -q '^active$'; then
  echo "active"
elif echo "$state" | grep -Eq '^(inactive|failed|activating|deactivating)$'; then
  echo "${state%%$'\n'*}"
elif systemctl list-unit-files 'actions.runner.*' 2>/dev/null | grep -q 'actions.runner.'; then
  echo "inactive"
else
  echo "missing"
fi
'''
    return ssh_capture(host, script).strip()


def cmd_status(args: argparse.Namespace) -> int:
    host = load_host_triplet()
    print(f"Host: {host.user}@{host.host}:{host.port}")

    remote_status = r'''
set -euo pipefail
status_out="$(systemctl --no-pager --full status 'actions.runner.*' 2>/dev/null || true)"
if [[ -n "${status_out}" ]]; then
  printf '%s\n' "${status_out}" | sed -n '1,60p'
else
  echo "No GitHub Actions runner service is installed on this host."
fi
'''
    ssh_script(host, remote_status)

    repo = args.repo
    print(f"\nGitHub runners for {repo} (label: ci-1focus):")
    out = gh_api(
        f"repos/{repo}/actions/runners",
        jq='[.runners[] | select(any(.labels[]; .name == "ci-1focus")) | "\\(.name)\\t\\(.status)\\tbusy=\\(.busy)"] | .[]?',
    )
    if out:
        print(out)
    else:
        print("No runners with label ci-1focus found.")
    return 0


def cmd_health(args: argparse.Namespace) -> int:
    host = load_host_triplet()
    runner_name = args.runner_name or default_runner_name(host)
    service = host_service_state(host)
    gh_status, busy = github_runner_state(args.repo, runner_name)
    busy_str = "n/a" if busy is None else ("true" if busy else "false")
    print(
        f"runner={runner_name} host_service={service} github_status={gh_status} busy={busy_str}"
    )
    return 0 if service == "active" and gh_status == "online" else 1


def cmd_wait_online(args: argparse.Namespace) -> int:
    host = load_host_triplet()
    runner_name = args.runner_name or default_runner_name(host)
    deadline = time.time() + max(1, args.timeout_secs)
    interval = max(1, args.interval_secs)

    while time.time() <= deadline:
        service = host_service_state(host)
        gh_status, busy = github_runner_state(args.repo, runner_name)
        busy_str = "n/a" if busy is None else ("true" if busy else "false")
        print(
            f"waiting: runner={runner_name} host_service={service} github_status={gh_status} busy={busy_str}"
        )
        if service == "active" and gh_status == "online":
            return 0
        time.sleep(interval)

    print(
        f"Timed out waiting for runner to become online: {runner_name}",
        file=sys.stderr,
    )
    return 1


def cmd_install(args: argparse.Namespace) -> int:
    host = load_host_triplet()
    repo = args.repo
    labels = args.labels
    runner_name = args.runner_name or default_runner_name(host)

    version = args.version
    if not version:
        latest = gh_api("repos/actions/runner/releases/latest", jq=".tag_name")
        version = latest.lstrip("v")

    registration_token = gh_api(
        f"repos/{repo}/actions/runners/registration-token",
        method="POST",
        jq=".token",
    )
    remove_token = gh_api(
        f"repos/{repo}/actions/runners/remove-token",
        method="POST",
        jq=".token",
    )

    setup_script = f'''
set -euo pipefail
{shell_assign("RUNNER_DIR", DEFAULT_RUNNER_DIR)}
{shell_assign("REPO", repo)}
{shell_assign("VERSION", version)}
{shell_assign("RUNNER_NAME", runner_name)}
{shell_assign("LABELS", labels)}
{shell_assign("REGISTRATION_TOKEN", registration_token)}
{shell_assign("REMOVE_TOKEN", remove_token)}

if [ "$(id -u)" -eq 0 ]; then
  SUDO=""
  RUNNER_USER_CMD="runuser -u gha-runner --"
else
  if ! command -v sudo >/dev/null 2>&1; then
    echo "sudo is required for non-root execution on the host" >&2
    exit 1
  fi
  SUDO="sudo"
  RUNNER_USER_CMD="sudo -u gha-runner"
fi

if command -v apt-get >/dev/null 2>&1; then
  $SUDO apt-get update -y
  $SUDO apt-get install -y curl ca-certificates tar
fi

if ! id -u gha-runner >/dev/null 2>&1; then
  $SUDO useradd --create-home --home-dir /home/gha-runner --shell /bin/bash gha-runner
fi

$SUDO mkdir -p "$RUNNER_DIR"
$SUDO chown -R gha-runner:gha-runner "$RUNNER_DIR"
cd "$RUNNER_DIR"

CURRENT_VERSION=""
if [ -f .runner_version ]; then
  CURRENT_VERSION="$(cat .runner_version || true)"
fi

if [ ! -x ./config.sh ] || [ "$CURRENT_VERSION" != "$VERSION" ]; then
  rm -rf "$RUNNER_DIR"/*
  curl -fsSL -o actions-runner.tar.gz "https://github.com/actions/runner/releases/download/v${{VERSION}}/actions-runner-linux-x64-${{VERSION}}.tar.gz"
  tar xzf actions-runner.tar.gz
  rm -f actions-runner.tar.gz
  echo "$VERSION" > .runner_version
  $SUDO chown -R gha-runner:gha-runner "$RUNNER_DIR"
fi

# Ensure re-install is idempotent: service must be removed before reconfiguration.
if [ -x ./svc.sh ]; then
  ./svc.sh stop || true
  ./svc.sh uninstall || true
fi

if [ -f .runner ]; then
  $RUNNER_USER_CMD env RUNNER_DIR="$RUNNER_DIR" REMOVE_TOKEN="$REMOVE_TOKEN" \
    bash -lc 'cd "$RUNNER_DIR" && ./config.sh remove --token "$REMOVE_TOKEN" || true'
fi

$RUNNER_USER_CMD env RUNNER_DIR="$RUNNER_DIR" REPO="$REPO" REGISTRATION_TOKEN="$REGISTRATION_TOKEN" RUNNER_NAME="$RUNNER_NAME" LABELS="$LABELS" \
  bash -lc 'cd "$RUNNER_DIR" && ./config.sh --url "https://github.com/$REPO" --token "$REGISTRATION_TOKEN" --name "$RUNNER_NAME" --labels "$LABELS" --work _work --unattended --replace'

cd "$RUNNER_DIR"
if [ -x ./svc.sh ]; then
  ./svc.sh install gha-runner || true
  ./svc.sh start
fi

systemctl --no-pager --full status 'actions.runner.*' | sed -n '1,60p' || true
'''

    print(f"Installing runner on {host.user}@{host.host}:{host.port}")
    print(f"Repo: {repo}")
    print(f"Runner name: {runner_name}")
    print(f"Labels: {labels}")
    print(f"Runner version: {version}")
    ssh_script(host, setup_script)
    return 0


def cmd_remove(args: argparse.Namespace) -> int:
    host = load_host_triplet()
    repo = args.repo
    remove_token = gh_api(
        f"repos/{repo}/actions/runners/remove-token",
        method="POST",
        jq=".token",
    )
    purge = "1" if args.purge else "0"

    remove_script = f'''
set -euo pipefail
{shell_assign("RUNNER_DIR", DEFAULT_RUNNER_DIR)}
{shell_assign("REMOVE_TOKEN", remove_token)}
{shell_assign("PURGE", purge)}

if [ "$(id -u)" -eq 0 ]; then
  SUDO=""
  RUNNER_USER_CMD="runuser -u gha-runner --"
else
  if ! command -v sudo >/dev/null 2>&1; then
    echo "sudo is required for non-root execution on the host" >&2
    exit 1
  fi
  SUDO="sudo"
  RUNNER_USER_CMD="sudo -u gha-runner"
fi

if [ ! -d "$RUNNER_DIR" ]; then
  echo "Runner directory not found: $RUNNER_DIR"
  exit 0
fi

cd "$RUNNER_DIR"
if [ -x ./svc.sh ]; then
  ./svc.sh stop || true
fi

if [ -f .runner ] && [ -x ./config.sh ]; then
  $RUNNER_USER_CMD env RUNNER_DIR="$RUNNER_DIR" REMOVE_TOKEN="$REMOVE_TOKEN" \
    bash -lc 'cd "$RUNNER_DIR" && ./config.sh remove --token "$REMOVE_TOKEN" || true'
fi

if [ -x ./svc.sh ]; then
  ./svc.sh uninstall || true
fi

if [ "$PURGE" = "1" ]; then
  cd /
  $SUDO rm -rf "$RUNNER_DIR"
  $SUDO userdel -r gha-runner || true
  echo "Runner files and gha-runner user removed."
else
  echo "Runner unregistered and service removed (files kept)."
fi
'''

    print(f"Removing runner from {host.user}@{host.host}:{host.port}")
    ssh_script(host, remove_script)
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Manage ci.1focus.ai host GitHub runner")
    sub = parser.add_subparsers(dest="command", required=True)

    status = sub.add_parser("status", help="Show remote service status + GitHub runner status")
    status.add_argument("--repo", default=DEFAULT_REPO, help="GitHub repo in owner/name format")
    status.set_defaults(handler=cmd_status)

    install = sub.add_parser("install", help="Install/register runner on configured infra Linux host")
    install.add_argument("--repo", default=DEFAULT_REPO, help="GitHub repo in owner/name format")
    install.add_argument("--runner-name", default="", help="Runner name override")
    install.add_argument("--labels", default=DEFAULT_LABELS, help="Comma-separated runner labels")
    install.add_argument("--version", default="", help="actions/runner version (default: latest)")
    install.set_defaults(handler=cmd_install)

    remove = sub.add_parser("remove", help="Unregister runner and remove service")
    remove.add_argument("--repo", default=DEFAULT_REPO, help="GitHub repo in owner/name format")
    remove.add_argument("--purge", action="store_true", help="Also delete runner files and gha-runner user")
    remove.set_defaults(handler=cmd_remove)

    health = sub.add_parser("health", help="Machine-friendly runner health check")
    health.add_argument("--repo", default=DEFAULT_REPO, help="GitHub repo in owner/name format")
    health.add_argument("--runner-name", default="", help="Runner name override")
    health.set_defaults(handler=cmd_health)

    wait_online = sub.add_parser("wait-online", help="Wait until runner is active and GitHub reports online")
    wait_online.add_argument("--repo", default=DEFAULT_REPO, help="GitHub repo in owner/name format")
    wait_online.add_argument("--runner-name", default="", help="Runner name override")
    wait_online.add_argument("--timeout-secs", type=int, default=120, help="Maximum wait time")
    wait_online.add_argument("--interval-secs", type=int, default=5, help="Polling interval")
    wait_online.set_defaults(handler=cmd_wait_online)

    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    try:
        return int(args.handler(args))
    except subprocess.CalledProcessError as exc:
        if exc.stderr:
            print(exc.stderr.strip(), file=sys.stderr)
        return exc.returncode or 1


if __name__ == "__main__":
    raise SystemExit(main())
