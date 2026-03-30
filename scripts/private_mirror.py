#!/usr/bin/env python3

from __future__ import annotations

import argparse
import json
import subprocess
from pathlib import Path


def run(
    argv: list[str],
    *,
    cwd: Path | None = None,
    capture: bool = False,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        argv,
        cwd=str(cwd) if cwd is not None else None,
        text=True,
        capture_output=capture,
    )
    if check and result.returncode != 0:
        if capture:
            stderr = result.stderr.strip()
            stdout = result.stdout.strip()
            message = stderr or stdout or f"command failed: {' '.join(argv)}"
            raise SystemExit(message)
        raise SystemExit(result.returncode)
    return result


def git_capture(repo_root: Path, *args: str) -> str:
    result = run(["git", *args], cwd=repo_root, capture=True)
    return result.stdout.strip()


def git_maybe_capture(repo_root: Path, *args: str) -> str | None:
    result = subprocess.run(
        ["git", *args],
        cwd=str(repo_root),
        text=True,
        capture_output=True,
    )
    if result.returncode != 0:
        return None
    value = result.stdout.strip()
    return value or None


def gh_capture(*args: str) -> str:
    result = run(["gh", *args], capture=True)
    return result.stdout.strip()


def parse_github_remote(url: str) -> tuple[str, str] | None:
    trimmed = url.strip().removesuffix("/")
    if trimmed.startswith("git@github.com:"):
        path = trimmed.removeprefix("git@github.com:").removesuffix(".git")
    elif trimmed.startswith("https://github.com/"):
        path = trimmed.removeprefix("https://github.com/").removesuffix(".git")
    else:
        return None

    if "/" not in path:
        return None
    owner, repo = path.split("/", 1)
    if not owner or not repo:
        return None
    return owner, repo


def normalize_git_url(url: str) -> str:
    return url.strip().removesuffix("/").removesuffix(".git")


def repo_root_from_arg(repo_root: str | None) -> Path:
    if repo_root:
        return Path(repo_root).expanduser().resolve()
    return Path.cwd().resolve()


def current_branch(repo_root: Path) -> str:
    branch = git_capture(repo_root, "rev-parse", "--abbrev-ref", "HEAD")
    if not branch or branch == "HEAD":
        raise SystemExit("detached HEAD; check out a branch first")
    return branch


def github_login() -> str:
    login = gh_capture("api", "user", "-q", ".login")
    if not login:
        raise SystemExit("could not determine GitHub login from gh")
    return login


def repo_defaults(repo_root: Path) -> tuple[str, str]:
    upstream_url = git_maybe_capture(repo_root, "remote", "get-url", "upstream")
    origin_url = git_maybe_capture(repo_root, "remote", "get-url", "origin")
    remote_url = upstream_url or origin_url
    if remote_url is None:
        raise SystemExit("could not determine base repo from upstream/origin")
    parsed = parse_github_remote(remote_url)
    if parsed is None:
        raise SystemExit(f"unsupported GitHub remote URL: {remote_url}")
    return parsed


def repo_public_remote(repo_root: Path) -> str:
    upstream_url = git_maybe_capture(repo_root, "remote", "get-url", "upstream")
    origin_url = git_maybe_capture(repo_root, "remote", "get-url", "origin")
    if upstream_url and origin_url and upstream_url.strip() != origin_url.strip():
        return "upstream"
    if origin_url:
        return "origin"
    if upstream_url:
        return "upstream"
    raise SystemExit("could not determine public remote from upstream/origin")


def infer_default_branch(repo_root: Path) -> str:
    remote = repo_public_remote(repo_root)
    head_ref = git_maybe_capture(repo_root, "symbolic-ref", f"refs/remotes/{remote}/HEAD")
    if head_ref and "/" in head_ref:
        return head_ref.rsplit("/", 1)[-1]
    remote_info = git_maybe_capture(repo_root, "remote", "show", remote)
    if remote_info:
        for line in remote_info.splitlines():
            stripped = line.strip()
            prefix = "HEAD branch: "
            if stripped.startswith(prefix):
                value = stripped.removeprefix(prefix).strip()
                if value and value != "(unknown)":
                    return value
    raise SystemExit(f"could not determine default branch for remote {remote}")


def ensure_remote(repo_root: Path, remote: str, target_url: str) -> None:
    existing_url = git_maybe_capture(repo_root, "remote", "get-url", remote)
    if existing_url == target_url:
        return
    if existing_url is None:
        run(["git", "remote", "add", remote, target_url], cwd=repo_root)
    else:
        run(["git", "remote", "set-url", remote, target_url], cwd=repo_root)


def is_shallow_repo(repo_root: Path) -> bool:
    return git_capture(repo_root, "rev-parse", "--is-shallow-repository") == "true"


def has_distinct_upstream_remote(repo_root: Path) -> bool:
    upstream_url = git_maybe_capture(repo_root, "config", "--get", "remote.upstream.url")
    if upstream_url is None:
        return False
    origin_url = git_maybe_capture(repo_root, "config", "--get", "remote.origin.url")
    if origin_url is None:
        return True
    return normalize_git_url(origin_url) != normalize_git_url(upstream_url)


def ensure_complete_history(repo_root: Path) -> None:
    primary_remote = repo_public_remote(repo_root)
    if not is_shallow_repo(repo_root):
        primary_remote = None
    if primary_remote is not None:
        run(["git", "fetch", "--unshallow", "--tags", primary_remote], cwd=repo_root)

    seen_remotes: set[str] = set()
    if primary_remote is not None:
        seen_remotes.add(primary_remote)

    for remote in ["origin", "upstream"]:
        if remote in seen_remotes:
            continue
        if git_maybe_capture(repo_root, "config", "--get", f"remote.{remote}.url") is None:
            continue
        if remote == "upstream" and not has_distinct_upstream_remote(repo_root):
            continue
        run(["git", "fetch", "--tags", remote], cwd=repo_root)


def gh_repo_exists(owner: str, repo: str) -> bool:
    result = subprocess.run(
        ["gh", "repo", "view", f"{owner}/{repo}", "--json", "nameWithOwner"],
        text=True,
        capture_output=True,
    )
    return result.returncode == 0


def ensure_private_repo(owner: str, repo: str) -> None:
    if gh_repo_exists(owner, repo):
        return
    run(
        [
            "gh",
            "api",
            "user/repos",
            "-f",
            f"name={repo}",
            "-F",
            "private=true",
            "-F",
            "has_wiki=false",
        ]
    )


def gh_repo_info(owner: str, repo: str) -> dict:
    raw = gh_capture("api", f"repos/{owner}/{repo}")
    return json.loads(raw)


def ensure_default_branch(owner: str, repo: str, branch: str) -> None:
    run(
        [
            "gh",
            "api",
            "-X",
            "PATCH",
            f"repos/{owner}/{repo}",
            "-f",
            f"default_branch={branch}",
        ]
    )


def set_push_remote(repo_root: Path, branch: str, remote: str) -> None:
    run(["git", "config", "remote.pushDefault", remote], cwd=repo_root)
    run(["git", "config", f"branch.{branch}.pushRemote", remote], cwd=repo_root)


def show_status(args: argparse.Namespace) -> None:
    repo_root = repo_root_from_arg(args.repo_root)
    branch = args.branch or current_branch(repo_root)
    owner = args.owner or github_login()
    _, upstream_repo = repo_defaults(repo_root)
    mirror_repo = args.repo or f"{upstream_repo}{args.suffix}"
    remote = args.remote

    info: dict | None = None
    if gh_repo_exists(owner, mirror_repo):
        info = gh_repo_info(owner, mirror_repo)

    status = {
        "branch": branch,
        "branch_push_remote": git_maybe_capture(
            repo_root, "config", "--get", f"branch.{branch}.pushRemote"
        ),
        "github_default_branch": None if info is None else info.get("default_branch"),
        "github_private": None if info is None else info.get("private"),
        "github_repo_exists": info is not None,
        "mirror_repo": mirror_repo,
        "owner": owner,
        "remote": remote,
        "remote_push_default": git_maybe_capture(
            repo_root, "config", "--get", "remote.pushDefault"
        ),
        "remote_url": git_maybe_capture(repo_root, "remote", "get-url", remote),
        "repo_root": str(repo_root),
    }
    print(json.dumps(status, indent=2, sort_keys=True))


def ensure_private_mirror(args: argparse.Namespace) -> None:
    repo_root = repo_root_from_arg(args.repo_root)
    branch = args.branch or current_branch(repo_root)
    default_branch = args.default_branch or infer_default_branch(repo_root)
    owner = args.owner or github_login()
    _, upstream_repo = repo_defaults(repo_root)
    mirror_repo = args.repo or f"{upstream_repo}{args.suffix}"
    remote = args.remote
    target_url = f"git@github.com:{owner}/{mirror_repo}.git"

    ensure_complete_history(repo_root)
    ensure_private_repo(owner, mirror_repo)
    ensure_remote(repo_root, remote, target_url)
    run(["git", "push", remote, branch], cwd=repo_root)
    set_push_remote(repo_root, default_branch, remote)
    if branch != default_branch:
        set_push_remote(repo_root, branch, remote)
    ensure_default_branch(owner, mirror_repo, default_branch)

    info = gh_repo_info(owner, mirror_repo)
    print(
        json.dumps(
            {
                "branch": branch,
                "default_branch": info.get("default_branch"),
                "private": info.get("private"),
                "remote": remote,
                "remote_url": target_url,
                "repo": info.get("full_name"),
                "repo_root": str(repo_root),
            },
            indent=2,
            sort_keys=True,
        )
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Create or repair a private GitHub mirror remote for the current repo."
    )
    sub = parser.add_subparsers(dest="command", required=True)

    def add_common_flags(target: argparse.ArgumentParser) -> None:
        target.add_argument("--repo-root", default=None, help="Repo root (defaults to cwd)")
        target.add_argument("--owner", default=None, help="GitHub owner (defaults to gh login)")
        target.add_argument("--repo", default=None, help="Mirror repo name (defaults to <repo>-i)")
        target.add_argument("--remote", default="fork", help="Git remote name (default: fork)")
        target.add_argument("--suffix", default="-i", help="Repo name suffix (default: -i)")
        target.add_argument("--branch", default=None, help="Branch to push (defaults to current branch)")

    ensure = sub.add_parser(
        "ensure",
        help="Create/update the private mirror repo, push the branch, and set the default branch.",
    )
    add_common_flags(ensure)
    ensure.add_argument(
        "--default-branch",
        default=None,
        help="GitHub default branch to set (defaults to the repo's public trunk branch)",
    )

    status = sub.add_parser(
        "status",
        help="Show local remote and GitHub private-mirror state.",
    )
    add_common_flags(status)

    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    if args.command == "ensure":
        ensure_private_mirror(args)
    elif args.command == "status":
        show_status(args)
    else:
        parser.error(f"unsupported command: {args.command}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
