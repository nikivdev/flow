#!/usr/bin/env python3
"""
Toggle CI workflows between runner profiles.

Profiles:
  - github: Default GitHub-hosted Linux jobs, SIMD lane disabled.
  - blacksmith: Blacksmith Linux jobs, SIMD lane enabled on Blacksmith.
  - host: GitHub Linux jobs, SIMD lane enabled on ci.1focus.ai self-hosted runner.

Usage:
  python3 scripts/ci_blacksmith.py status
  python3 scripts/ci_blacksmith.py enable
  python3 scripts/ci_blacksmith.py enable --commit --push
  python3 scripts/ci_blacksmith.py host
  python3 scripts/ci_blacksmith.py disable
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = [
    ROOT / ".github" / "workflows" / "canary.yml",
    ROOT / ".github" / "workflows" / "release.yml",
]

GITHUB_X64 = "ubuntu-latest"
GITHUB_ARM = "ubuntu-latest"
BLACKSMITH_X64 = "blacksmith-2vcpu-ubuntu-2404"
BLACKSMITH_ARM = "blacksmith-2vcpu-ubuntu-2404-arm"
BLACKSMITH_SIMD = "blacksmith-4vcpu-ubuntu-2404"
HOST_SIMD = "[self-hosted, linux, x64, ci-1focus]"

WORKFLOW_REL_PATHS = [
    ".github/workflows/canary.yml",
    ".github/workflows/release.yml",
]


def rewrite_workflow(path: Path, mode: str) -> bool:
    content = path.read_text(encoding="utf-8")
    original = content

    if mode == "blacksmith":
        linux_x64 = BLACKSMITH_X64
        linux_arm = BLACKSMITH_ARM
        simd_runs_on = BLACKSMITH_SIMD
        simd_if_line = ""
    elif mode == "host":
        linux_x64 = GITHUB_X64
        linux_arm = GITHUB_ARM
        simd_runs_on = HOST_SIMD
        simd_if_line = ""
    else:
        linux_x64 = GITHUB_X64
        linux_arm = GITHUB_ARM
        simd_runs_on = "ubuntu-latest"
        simd_if_line = "    if: ${{ false }}\n"

    content = re.sub(
        r"(- target: x86_64-unknown-linux-gnu\s*\n\s*os:\s*)([^\n]+)",
        rf"\1{linux_x64}",
        content,
        count=1,
    )
    content = re.sub(
        r"(- target: aarch64-unknown-linux-gnu\s*\n\s*os:\s*)([^\n]+)",
        rf"\1{linux_arm}",
        content,
        count=1,
    )

    simd_block = re.compile(
        r"(^  build-linux-host-simd:\n)(?P<body>(?:^    .*\n)*)",
        re.MULTILINE,
    )
    block_match = simd_block.search(content)
    if block_match:
        body_lines = block_match.group("body").splitlines(keepends=True)
        rewritten_body: list[str] = []
        has_runs_on = False
        for line in body_lines:
            if re.match(r"^    if:", line):
                continue
            if re.match(r"^    runs-on:", line):
                rewritten_body.append(f"    runs-on: {simd_runs_on}\n")
                has_runs_on = True
                continue
            rewritten_body.append(line)

        if not has_runs_on:
            rewritten_body.insert(0, f"    runs-on: {simd_runs_on}\n")
        if simd_if_line:
            rewritten_body.insert(0, simd_if_line)

        replacement = block_match.group(1) + "".join(rewritten_body)
        content = (
            content[: block_match.start()]
            + replacement
            + content[block_match.end() :]
        )

    changed = content != original
    if changed:
        path.write_text(content, encoding="utf-8")
    return changed


def detect_profile(path: Path) -> str:
    content = path.read_text(encoding="utf-8")
    if BLACKSMITH_X64 in content and BLACKSMITH_ARM in content:
        return "blacksmith"
    if HOST_SIMD in content and "if: ${{ false }}" not in content:
        return "host"
    if GITHUB_X64 in content and "blacksmith-" not in content:
        return "github"
    if GITHUB_X64 in content and GITHUB_ARM in content:
        return "github"
    return "mixed"


def status() -> int:
    all_ok = True
    for wf in WORKFLOWS:
        profile = detect_profile(wf)
        print(f"{wf.relative_to(ROOT)}: {profile}")
        if profile == "mixed":
            all_ok = False
    if not all_ok:
        print("Detected mixed workflow state; run enable or disable to normalize.")
        return 1
    return 0


def run_cmd(args: list[str]) -> None:
    subprocess.run(args, cwd=ROOT, check=True)


def has_staged_workflow_changes() -> bool:
    result = subprocess.run(
        ["git", "diff", "--cached", "--quiet", "--", *WORKFLOW_REL_PATHS],
        cwd=ROOT,
        check=False,
    )
    return result.returncode != 0


def maybe_commit_and_push(mode: str, commit: bool, push: bool) -> int:
    if push and not commit:
        print("--push requires --commit", file=sys.stderr)
        return 2

    if not commit:
        return 0

    run_cmd(["git", "add", *WORKFLOW_REL_PATHS])
    if not has_staged_workflow_changes():
        print("No workflow changes to commit.")
        return 0

    run_cmd(["git", "commit", "-m", f"ci: switch workflows to {mode} runners"])
    print("Committed workflow changes.")

    if push:
        run_cmd(["git", "push", "origin", "HEAD"])
        print("Pushed commit.")

    return 0


def set_mode(mode: str, commit: bool, push: bool) -> int:
    if push and not commit:
        print("--push requires --commit", file=sys.stderr)
        return 2

    changed_any = False
    for wf in WORKFLOWS:
        changed = rewrite_workflow(wf, mode=mode)
        changed_any = changed_any or changed
        state = "updated" if changed else "unchanged"
        print(f"{wf.relative_to(ROOT)}: {state}")

    if changed_any:
        print(f"CI runner mode set to: {mode}")
    else:
        print(f"CI runner mode already set to: {mode}")
    return maybe_commit_and_push(mode=mode, commit=commit, push=push)


def main() -> int:
    parser = argparse.ArgumentParser(description="Manage CI runner profile.")
    parser.add_argument(
        "command",
        choices=["status", "enable", "disable", "host"],
        help="status | enable (Blacksmith) | host (self-hosted SIMD lane) | disable (GitHub-hosted)",
    )
    parser.add_argument(
        "--commit",
        action="store_true",
        help="Commit workflow changes after rewriting files",
    )
    parser.add_argument(
        "--push",
        action="store_true",
        help="Push the commit (requires --commit)",
    )
    args = parser.parse_args()

    if args.command == "status":
        return status()
    if args.command == "enable":
        return set_mode(mode="blacksmith", commit=args.commit, push=args.push)
    if args.command == "host":
        return set_mode(mode="host", commit=args.commit, push=args.push)
    if args.command == "disable":
        return set_mode(mode="github", commit=args.commit, push=args.push)
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
