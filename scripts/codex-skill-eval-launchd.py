#!/usr/bin/env python3
import argparse
import os
import plistlib
import shutil
import subprocess
import sys
from pathlib import Path


LABEL = "dev.nikiv.flow-codex-skill-eval"


def run(cmd: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, text=True, capture_output=True, check=False)


def resolve_f_bin(repo_root: Path) -> str:
    env_override = os.environ.get("FLOW_CODEX_SKILL_EVAL_F_BIN", "").strip()
    if env_override:
        return env_override
    which_f = shutil.which("f")
    if which_f:
        return which_f
    for candidate in [
        repo_root / "target" / "release" / "f",
        repo_root / "target" / "debug" / "f",
    ]:
        if candidate.exists():
            return str(candidate)
    raise SystemExit("Could not resolve f binary. Build flow first or set FLOW_CODEX_SKILL_EVAL_F_BIN.")


def plist_path() -> Path:
    return Path.home() / "Library" / "LaunchAgents" / f"{LABEL}.plist"


def domain_target() -> str:
    return f"gui/{os.getuid()}/{LABEL}"


def log_dir() -> Path:
    path = Path.home() / ".flow" / "logs"
    path.mkdir(parents=True, exist_ok=True)
    return path


def install(
    repo_root: Path,
    minutes: int,
    limit: int,
    max_targets: int,
    within_hours: int,
    dry_run: bool,
) -> int:
    if minutes < 5:
        raise SystemExit("--minutes must be at least 5")
    if limit < 1 or max_targets < 1 or within_hours < 1:
        raise SystemExit("--limit, --max-targets, and --within-hours must be positive")

    f_bin = resolve_f_bin(repo_root)
    p = plist_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    logs = log_dir()

    payload = {
        "Label": LABEL,
        "ProgramArguments": [
            f_bin,
            "codex",
            "skill-eval",
            "cron",
            "--limit",
            str(limit),
            "--max-targets",
            str(max_targets),
            "--within-hours",
            str(within_hours),
        ],
        "RunAtLoad": True,
        "StartInterval": minutes * 60,
        "ProcessType": "Background",
        "StandardOutPath": str(logs / "codex-skill-eval.launchd.stdout.log"),
        "StandardErrorPath": str(logs / "codex-skill-eval.launchd.stderr.log"),
        "EnvironmentVariables": {
            "PATH": "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        },
    }
    if dry_run:
        print(f"plist:  {p}")
        print(f"f_bin:  {f_bin}")
        print(f"every:  {minutes} minutes")
        print(f"limit:  {limit}")
        print(f"max_targets: {max_targets}")
        print(f"within_hours: {within_hours}")
        print(plistlib.dumps(payload).decode("utf-8"), end="")
        return 0
    with p.open("wb") as f:
        plistlib.dump(payload, f)

    run(["launchctl", "bootout", f"gui/{os.getuid()}", str(p)])
    b = run(["launchctl", "bootstrap", f"gui/{os.getuid()}", str(p)])
    if b.returncode != 0:
        print(b.stderr.strip(), file=sys.stderr)
        return b.returncode
    run(["launchctl", "kickstart", "-k", domain_target()])
    print(f"loaded: {domain_target()}")
    print(f"plist:  {p}")
    print(f"f_bin:  {f_bin}")
    print(f"every:  {minutes} minutes")
    print(f"limit:  {limit}")
    print(f"max_targets: {max_targets}")
    print(f"within_hours: {within_hours}")
    return 0


def uninstall() -> int:
    p = plist_path()
    run(["launchctl", "bootout", f"gui/{os.getuid()}", str(p)])
    if p.exists():
        p.unlink()
    print(f"unloaded: {domain_target()}")
    print(f"removed:  {p}")
    return 0


def status() -> int:
    out = run(["launchctl", "print", domain_target()])
    if out.returncode != 0:
        print(f"{domain_target()}: not loaded")
        if out.stderr.strip():
            print(out.stderr.strip())
        return 0
    print(out.stdout, end="")
    return 0


def logs(lines: int) -> int:
    stdout = log_dir() / "codex-skill-eval.launchd.stdout.log"
    stderr = log_dir() / "codex-skill-eval.launchd.stderr.log"
    for path in [stdout, stderr]:
        print(f"==> {path}")
        if not path.exists():
            print("(missing)")
            continue
        text = path.read_text(encoding="utf-8", errors="replace").splitlines()
        for line in text[-lines:]:
            print(line)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Manage launchd schedule for Flow Codex skill-eval cron."
    )
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_install = sub.add_parser("install")
    p_install.add_argument("--minutes", type=int, default=30)
    p_install.add_argument("--limit", type=int, default=400)
    p_install.add_argument("--max-targets", type=int, default=12)
    p_install.add_argument("--within-hours", type=int, default=168)
    p_install.add_argument("--dry-run", action="store_true")

    sub.add_parser("uninstall")
    sub.add_parser("status")

    p_logs = sub.add_parser("logs")
    p_logs.add_argument("--lines", type=int, default=120)

    args = parser.parse_args()
    repo_root = Path(__file__).resolve().parents[1]
    if args.cmd == "install":
        return install(
            repo_root,
            args.minutes,
            args.limit,
            args.max_targets,
            args.within_hours,
            args.dry_run,
        )
    if args.cmd == "uninstall":
        return uninstall()
    if args.cmd == "status":
        return status()
    if args.cmd == "logs":
        return logs(args.lines)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
