#!/usr/bin/env python3

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path


HOME = Path.home()
DEFAULT_REPO_ROOT = Path(
    os.environ.get(
        "FLOW_CODEX_J_REPO",
        str(HOME / "repos" / "openai" / "codex"),
    )
).expanduser()
STATE_DIR = Path(
    os.environ.get(
        "FLOW_CODEX_FORK_STATE_DIR",
        str(HOME / ".flow" / "codex-fork"),
    )
).expanduser()
DEFAULT_INFRA_BIN = os.environ.get("FLOW_CODEX_J_INFRA_BIN", "infra")
DEFAULT_BUILDER = os.environ.get("FLOW_CODEX_J_BUILDER", "j-mac-1")
DEFAULT_PROFILE = os.environ.get("FLOW_CODEX_J_PROFILE", "release")
HOME_BRANCH = "j"
TRUNK_BRANCH = "main"
PUBLIC_REMOTE = "origin"
PUSH_REMOTE = "private"
DARWIN_TARGET = "aarch64-apple-darwin"
CODEX_PACKAGE = "codex-cli"


def fail(message: str, code: int = 1) -> int:
    print(f"Error: {message}", file=sys.stderr)
    return code


def run(
    cmd: list[str],
    *,
    cwd: Path,
    capture: bool = False,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        cmd,
        cwd=str(cwd),
        text=True,
        capture_output=capture,
        check=False,
    )
    if check and result.returncode != 0:
        if capture and result.stderr:
            print(result.stderr.rstrip(), file=sys.stderr)
        raise SystemExit(result.returncode)
    return result


def capture(cmd: list[str], *, cwd: Path) -> str:
    return run(cmd, cwd=cwd, capture=True).stdout.strip()


def resolve_repo_root(path_arg: str | None) -> Path:
    if path_arg:
        return Path(path_arg).expanduser().resolve()
    return DEFAULT_REPO_ROOT.resolve()


def codex_rs_root(repo_root: Path) -> Path:
    return repo_root / "codex-rs"


def managed_binary_path() -> Path:
    return STATE_DIR / "bin" / "current" / "codex"


def managed_dev_binary_path() -> Path:
    return STATE_DIR / "bin" / "current-dev" / "codex"


def managed_binary_target(path: Path) -> str | None:
    if not path.exists():
        return None
    try:
        return str(path.resolve())
    except OSError:
        return str(path)


def ensure_repo(repo_root: Path) -> None:
    if not repo_root.exists():
        raise SystemExit(fail(f"repo does not exist: {repo_root}"))
    probe = run(
        ["git", "rev-parse", "--is-inside-work-tree"],
        cwd=repo_root,
        capture=True,
        check=False,
    )
    if probe.returncode != 0 or probe.stdout.strip() != "true":
        raise SystemExit(fail(f"not a git checkout: {repo_root}"))
    if not (repo_root / ".jj").exists():
        raise SystemExit(fail(f"not a colocated jj checkout: {repo_root}"))


def ensure_branch_exists(repo_root: Path, branch: str) -> None:
    result = run(
        ["git", "show-ref", "--verify", "--quiet", f"refs/heads/{branch}"],
        cwd=repo_root,
        check=False,
    )
    if result.returncode != 0:
        raise SystemExit(fail(f"branch `{branch}` does not exist in {repo_root}"))


def ensure_clean(repo_root: Path) -> None:
    status = capture(["git", "status", "--porcelain"], cwd=repo_root)
    if status:
        raise SystemExit(
            fail(
                "home checkout is dirty; finish, squash, or move the work before running `f codex-j-sync`"
            )
        )


def ensure_home_attached(repo_root: Path) -> None:
    branch = capture(["git", "branch", "--show-current"], cwd=repo_root)
    if branch == HOME_BRANCH:
        return
    if branch and branch != HOME_BRANCH:
        raise SystemExit(
            fail(f"current branch is `{branch}`; switch back to `{HOME_BRANCH}` before syncing")
        )
    run(["git", "switch", HOME_BRANCH], cwd=repo_root)


def ensure_local_config(repo_root: Path) -> None:
    pairs = [
        ("flow.homeBranch", HOME_BRANCH),
        ("flow.publicRemote", PUBLIC_REMOTE),
        (f"branch.{HOME_BRANCH}.remote", PUBLIC_REMOTE),
        (f"branch.{HOME_BRANCH}.merge", f"refs/heads/{TRUNK_BRANCH}"),
        (f"branch.{HOME_BRANCH}.pushRemote", PUSH_REMOTE),
        ("remote.pushDefault", PUSH_REMOTE),
    ]
    for key, value in pairs:
        run(["git", "config", "--local", key, value], cwd=repo_root)


def ensure_main_tracks_origin(repo_root: Path) -> None:
    run(
        ["jj", "bookmark", "set", TRUNK_BRANCH, "-r", f"{TRUNK_BRANCH}@{PUBLIC_REMOTE}"],
        cwd=repo_root,
    )


def conflict_revisions(repo_root: Path) -> str:
    return capture(
        [
            "jj",
            "log",
            "-r",
            "conflicts()",
            "--no-graph",
            "-T",
            "commit_id.short() ++ \" \" ++ description.first_line() ++ \"\\n\"",
        ],
        cwd=repo_root,
    )


def show_status(repo_root: Path) -> None:
    managed_bin = managed_binary_path()
    print(f"managed_binary={managed_bin}", flush=True)
    target = managed_binary_target(managed_bin)
    if target:
        print(f"managed_binary_target={target}", flush=True)
    else:
        print("managed_binary_target=(missing)", flush=True)
    managed_dev_bin = managed_dev_binary_path()
    print(f"managed_dev_binary={managed_dev_bin}", flush=True)
    dev_target = managed_binary_target(managed_dev_bin)
    if dev_target:
        print(f"managed_dev_binary_target={dev_target}", flush=True)
    else:
        print("managed_dev_binary_target=(missing)", flush=True)
    run(["flow", "status"], cwd=repo_root)


def build(
    repo_root: Path,
    *,
    builder: str | None,
    profile: str,
    release: bool,
    install_as: str,
    push_cache: bool,
) -> None:
    ensure_repo(repo_root)
    cargo_root = codex_rs_root(repo_root)
    if not cargo_root.exists():
        raise SystemExit(fail(f"missing codex-rs workspace under {repo_root}"))

    cmd = [
        DEFAULT_INFRA_BIN,
        "build",
        "rust",
        str(cargo_root),
        "--package",
        CODEX_PACKAGE,
        "--bin",
        "codex",
        "--target",
        DARWIN_TARGET,
        "--sign-local",
        "--install-as",
        install_as,
    ]
    resolved_builder = builder or DEFAULT_BUILDER
    if resolved_builder:
        cmd.extend(["--builder", resolved_builder])
    if release or profile == "release":
        cmd.append("--release")
    else:
        cmd.extend(["--profile", profile])
    if push_cache:
        cmd.append("--push-cache")

    run(cmd, cwd=repo_root)


def sync(repo_root: Path, push: bool) -> None:
    ensure_repo(repo_root)
    ensure_branch_exists(repo_root, HOME_BRANCH)
    ensure_clean(repo_root)
    ensure_home_attached(repo_root)
    ensure_local_config(repo_root)
    ensure_main_tracks_origin(repo_root)

    cmd = ["flow", "jj", "sync", "--bookmark", HOME_BRANCH]
    if not push:
        cmd.append("--no-push")
    run(cmd, cwd=repo_root)

    conflicts = conflict_revisions(repo_root)
    if conflicts:
        raise SystemExit(
            fail(
                "jj sync completed with conflicts still present:\n" + conflicts.rstrip()
            )
        )

    run(["jj", "new", HOME_BRANCH], cwd=repo_root)
    run(["git", "switch", HOME_BRANCH], cwd=repo_root)
    show_status(repo_root)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Manage the personal `j` home-branch workflow for ~/repos/openai/codex"
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    status_parser = subparsers.add_parser(
        "status",
        help="Show Flow status for the Codex `j` home-branch checkout",
    )
    status_parser.add_argument(
        "--repo-root",
        help=f"Repo root (default: {DEFAULT_REPO_ROOT})",
    )

    sync_parser = subparsers.add_parser(
        "sync",
        help="Sync origin/main into `j`, then normalize the JJ/Git checkout back onto `j`",
    )
    sync_parser.add_argument(
        "--repo-root",
        help=f"Repo root (default: {DEFAULT_REPO_ROOT})",
    )
    sync_parser.add_argument(
        "--push",
        action="store_true",
        help="Push `j` to the configured push remote after sync",
    )

    build_parser = subparsers.add_parser(
        "build",
        help="Build Codex remotely for Darwin, sign locally, and promote it for `j` or `jdev`",
    )
    build_parser.add_argument(
        "--repo-root",
        help=f"Repo root (default: {DEFAULT_REPO_ROOT})",
    )
    build_parser.add_argument(
        "--builder",
        help="Named infra builder to use (defaults to FLOW_CODEX_J_BUILDER or infra default builder)",
    )
    build_parser.add_argument(
        "--profile",
        default=DEFAULT_PROFILE,
        help=f"Cargo profile for the remote build (default: {DEFAULT_PROFILE})",
    )
    build_parser.add_argument(
        "--release",
        action="store_true",
        help="Force the release profile",
    )
    build_parser.add_argument(
        "--push-cache",
        action="store_true",
        help="Push the built artifact into the configured infra cache",
    )
    build_parser.add_argument(
        "--dev",
        action="store_true",
        help="Build the dev/debug profile and install it as `jdev`",
    )
    build_parser.add_argument(
        "--install-as",
        choices=["j", "jdev"],
        help="Managed install slot to update (defaults to `j`, or `jdev` with --dev)",
    )

    args = parser.parse_args()
    repo_root = resolve_repo_root(getattr(args, "repo_root", None))

    if args.command == "status":
        ensure_repo(repo_root)
        ensure_branch_exists(repo_root, HOME_BRANCH)
        ensure_local_config(repo_root)
        show_status(repo_root)
        return 0

    if args.command == "sync":
        sync(repo_root, push=args.push)
        return 0

    if args.command == "build":
        install_as = args.install_as or ("jdev" if args.dev else "j")
        profile = args.profile
        if args.dev and profile == DEFAULT_PROFILE and not args.release:
            profile = "dev"
        build(
            repo_root,
            builder=args.builder,
            profile=profile,
            release=args.release,
            install_as=install_as,
            push_cache=args.push_cache,
        )
        return 0

    raise SystemExit(fail(f"unsupported command: {args.command}"))


if __name__ == "__main__":
    raise SystemExit(main())
