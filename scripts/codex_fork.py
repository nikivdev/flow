#!/usr/bin/env python3

from __future__ import annotations

import argparse
import os
import re
import shlex
import subprocess
import sys
from pathlib import Path


def env_path(name: str, default: Path) -> Path:
    value = os.environ.get(name)
    if not value:
        return default
    return Path(value).expanduser()


HOME = Path.home()
UPSTREAM_CHECKOUT = env_path(
    "FLOW_CODEX_UPSTREAM_CHECKOUT",
    HOME / "repos" / "openai" / "codex",
)
FORK_HOME = env_path(
    "FLOW_CODEX_FORK_HOME",
    HOME / "repos" / "nikivdev" / "codex",
)
WORKTREE_ROOT = env_path(
    "FLOW_CODEX_WORKTREE_ROOT",
    HOME / ".worktrees" / "codex",
)
WORKFLOW_DOC = env_path(
    "FLOW_CODEX_WORKFLOW_DOC",
    HOME / "docs" / "codex" / "codex-fork-home-branch-workflow.md",
)
STATE_DIR = env_path(
    "FLOW_CODEX_FORK_STATE_DIR",
    HOME / ".flow" / "codex-fork",
)
LAST_WORKTREE_FILE = STATE_DIR / "last-worktree.txt"
DEFAULT_BASE_BRANCH = os.environ.get("FLOW_CODEX_FORK_BASE_BRANCH", "nikiv")
DEFAULT_BRANCH_PREFIX = os.environ.get("FLOW_CODEX_FORK_BRANCH_PREFIX", "codex")
DEFAULT_REVIEW_PREFIX = os.environ.get("FLOW_CODEX_FORK_REVIEW_PREFIX", "review/nikiv")
DEFAULT_PRIVATE_REMOTE = os.environ.get("FLOW_CODEX_FORK_PRIVATE_REMOTE", "private")
DEFAULT_UPSTREAM_REMOTE = os.environ.get("FLOW_CODEX_FORK_UPSTREAM_REMOTE", "upstream")
DEFAULT_UPSTREAM_BRANCH = os.environ.get("FLOW_CODEX_FORK_UPSTREAM_BRANCH", "main")


def fail(message: str, code: int = 1) -> int:
    print(f"Error: {message}", file=sys.stderr)
    return code


def run(
    cmd: list[str],
    *,
    cwd: Path | None = None,
    capture: bool = False,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        cmd,
        cwd=str(cwd) if cwd is not None else None,
        text=True,
        capture_output=capture,
        check=False,
    )
    if check and result.returncode != 0:
        if capture and result.stderr:
            print(result.stderr.rstrip(), file=sys.stderr)
        raise SystemExit(result.returncode)
    return result


def capture(cmd: list[str], *, cwd: Path | None = None, check: bool = True) -> str:
    result = run(cmd, cwd=cwd, capture=True, check=check)
    return result.stdout.strip()


def ensure_repo(path: Path, label: str) -> None:
    if not path.exists():
        raise SystemExit(fail(f"{label} does not exist: {path}"))
    probe = run(
        ["git", "rev-parse", "--is-inside-work-tree"],
        cwd=path,
        capture=True,
        check=False,
    )
    if probe.returncode != 0 or probe.stdout.strip() != "true":
        raise SystemExit(fail(f"{label} is not a git checkout: {path}"))


def ensure_state_dir() -> None:
    STATE_DIR.mkdir(parents=True, exist_ok=True)


def read_last_worktree() -> Path | None:
    if not LAST_WORKTREE_FILE.exists():
        return None
    value = LAST_WORKTREE_FILE.read_text().strip()
    if not value:
        return None
    return Path(value).expanduser()


def write_last_worktree(path: Path) -> None:
    ensure_state_dir()
    LAST_WORKTREE_FILE.write_text(f"{path}\n")


def slugify(text: str) -> str:
    slug = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
    slug = re.sub(r"-{2,}", "-", slug)
    if not slug:
        raise SystemExit(fail(f"could not derive a branch slug from query: {text!r}"))
    return slug


def branch_to_worktree_name(branch: str) -> str:
    return branch.replace("/", "-")


def git_ref_exists(repo: Path, ref: str) -> bool:
    result = run(
        ["git", "show-ref", "--verify", "--quiet", ref],
        cwd=repo,
        check=False,
    )
    return result.returncode == 0


def git_branch_exists(repo: Path, branch: str) -> bool:
    return git_ref_exists(repo, f"refs/heads/{branch}")


def git_current_branch(repo: Path) -> str:
    branch = capture(["git", "branch", "--show-current"], cwd=repo)
    if not branch:
        raise SystemExit(fail(f"could not resolve current branch in {repo}"))
    return branch


def git_rev(repo: Path, ref: str) -> str | None:
    result = run(["git", "rev-parse", "--verify", ref], cwd=repo, capture=True, check=False)
    if result.returncode != 0:
        return None
    return result.stdout.strip()


def worktree_entries(repo: Path) -> list[dict[str, str]]:
    output = capture(["git", "worktree", "list", "--porcelain"], cwd=repo)
    entries: list[dict[str, str]] = []
    current: dict[str, str] = {}
    for line in output.splitlines():
        if not line:
            if current:
                entries.append(current)
                current = {}
            continue
        key, _, value = line.partition(" ")
        if key == "worktree":
            current["path"] = value
        elif key == "branch":
            current["branch"] = value.removeprefix("refs/heads/")
        elif key == "HEAD":
            current["head"] = value
        elif key == "detached":
            current["detached"] = "true"
    if current:
        entries.append(current)
    return entries


def worktree_for_branch(repo: Path, branch: str) -> Path | None:
    for entry in worktree_entries(repo):
        if entry.get("branch") == branch:
            return Path(entry["path"])
    return None


def branch_from_target(target: str) -> str:
    if target.startswith("codex/") or target.startswith("review/"):
        return target
    return f"{DEFAULT_BRANCH_PREFIX}/{slugify(target)}"


def default_worktree_path(branch: str) -> Path:
    return WORKTREE_ROOT / branch_to_worktree_name(branch)


def ensure_task_worktree(branch: str, path: Path, base: str) -> tuple[Path, bool]:
    existing = worktree_for_branch(FORK_HOME, branch)
    if existing is not None:
        return existing, True

    if path.exists() and not (path / ".git").exists():
        if any(path.iterdir()):
            raise SystemExit(
                fail(f"requested worktree path already exists and is not empty: {path}")
            )

    path.parent.mkdir(parents=True, exist_ok=True)
    if git_branch_exists(FORK_HOME, branch):
        run(["git", "worktree", "add", str(path), branch], cwd=FORK_HOME)
    else:
        run(["git", "worktree", "add", "-b", branch, str(path), base], cwd=FORK_HOME)
    return path, False


def build_prompt(query: str, branch: str, worktree: Path, base: str) -> str:
    return "\n".join(
        [
            f"Read {WORKFLOW_DOC} and make plan first.",
            "",
            f"Task: {query}",
            f"Branch: {branch}",
            f"Worktree: {worktree}",
            f"Base branch: {base}",
            f"Fork home checkout: {FORK_HOME}",
            f"Upstream reference checkout: {UPSTREAM_CHECKOUT}",
            "Keep the work scoped to this branch/worktree and do not touch unrelated fork worktrees.",
        ]
    )


def launch_codex_new(worktree: Path, prompt: str) -> int:
    cmd = [
        "codex",
        "--cd",
        str(worktree),
        "--yolo",
        "--sandbox",
        "danger-full-access",
        prompt,
    ]
    return subprocess.run(cmd, check=False).returncode


def launch_codex_resume_last(worktree: Path, prompt: str | None = None) -> int:
    cmd = [
        "codex",
        "--cd",
        str(worktree),
        "resume",
        "--last",
        "--dangerously-bypass-approvals-and-sandbox",
    ]
    if prompt:
        cmd.append(prompt)
    return subprocess.run(cmd, check=False).returncode


def print_next_commands(worktree: Path, branch: str, prompt: str) -> None:
    print(f"branch:   {branch}")
    print(f"worktree: {worktree}")
    print()
    print("next:")
    print(f"  cd {shlex.quote(str(worktree))}")
    print(
        "  "
        + shlex.join(
            [
                "codex",
                "--cd",
                str(worktree),
                "--yolo",
                "--sandbox",
                "danger-full-access",
                prompt,
            ]
        )
    )


def cmd_status(_args: argparse.Namespace) -> int:
    ensure_repo(FORK_HOME, "codex fork home checkout")
    print("# Codex fork workflow")
    print(f"upstream checkout: {UPSTREAM_CHECKOUT}")
    print(f"fork home:         {FORK_HOME}")
    print(f"worktree root:     {WORKTREE_ROOT}")
    print(f"workflow doc:      {WORKFLOW_DOC}")
    print()

    nikiv_sha = git_rev(FORK_HOME, DEFAULT_BASE_BRANCH)
    upstream_sha = git_rev(FORK_HOME, f"{DEFAULT_UPSTREAM_REMOTE}/{DEFAULT_UPSTREAM_BRANCH}")
    private_sha = git_rev(FORK_HOME, f"{DEFAULT_PRIVATE_REMOTE}/{DEFAULT_BASE_BRANCH}")

    print("# Branch heads")
    print(f"{DEFAULT_BASE_BRANCH}: {nikiv_sha or 'missing'}")
    print(f"{DEFAULT_UPSTREAM_REMOTE}/{DEFAULT_UPSTREAM_BRANCH}: {upstream_sha or 'missing'}")
    print(f"{DEFAULT_PRIVATE_REMOTE}/{DEFAULT_BASE_BRANCH}: {private_sha or 'missing'}")
    print()

    print("# Remotes")
    print(capture(["git", "remote", "-v"], cwd=FORK_HOME))
    print()

    print("# Worktrees")
    for entry in worktree_entries(FORK_HOME):
        branch = entry.get("branch", "(detached)")
        print(f"{branch:32} {entry['path']}")
    print()

    last = read_last_worktree()
    print("# Last worktree")
    if last is None:
        print("none recorded")
        return 0

    print(last)
    if last.exists():
        print()
        print("# Last worktree status")
        status = capture(["git", "status", "-sb"], cwd=last, check=False)
        if status:
            print(status)
    return 0


def cmd_sync(args: argparse.Namespace) -> int:
    ensure_repo(FORK_HOME, "codex fork home checkout")
    status = capture(["git", "status", "--porcelain"], cwd=FORK_HOME)
    if status:
        return fail(
            f"{FORK_HOME} is dirty; clean or stash it before syncing {DEFAULT_BASE_BRANCH}"
        )

    run(
        ["git", "fetch", DEFAULT_UPSTREAM_REMOTE, DEFAULT_UPSTREAM_BRANCH],
        cwd=FORK_HOME,
    )
    if git_ref_exists(FORK_HOME, f"refs/remotes/{DEFAULT_PRIVATE_REMOTE}/{DEFAULT_BASE_BRANCH}"):
        run(["git", "fetch", DEFAULT_PRIVATE_REMOTE, DEFAULT_BASE_BRANCH], cwd=FORK_HOME)

    run(["git", "switch", DEFAULT_BASE_BRANCH], cwd=FORK_HOME)
    run(
        ["git", "merge", "--ff-only", f"{DEFAULT_UPSTREAM_REMOTE}/{DEFAULT_UPSTREAM_BRANCH}"],
        cwd=FORK_HOME,
    )
    if args.push:
        run(["git", "push", DEFAULT_PRIVATE_REMOTE, DEFAULT_BASE_BRANCH], cwd=FORK_HOME)

    print(f"{DEFAULT_BASE_BRANCH} now matches {DEFAULT_UPSTREAM_REMOTE}/{DEFAULT_UPSTREAM_BRANCH}")
    if args.push:
        print(f"pushed {DEFAULT_BASE_BRANCH} to {DEFAULT_PRIVATE_REMOTE}")
    return 0


def cmd_task(args: argparse.Namespace) -> int:
    ensure_repo(FORK_HOME, "codex fork home checkout")
    branch = args.branch or branch_from_target(args.query)
    worktree = Path(args.path).expanduser() if args.path else default_worktree_path(branch)
    worktree, reused = ensure_task_worktree(branch, worktree, args.base)
    write_last_worktree(worktree)

    prompt = build_prompt(args.query, branch, worktree, args.base)
    print(f"branch:   {branch}")
    print(f"worktree: {worktree}")
    print(f"mode:     {'resume-or-new' if not args.new else 'new'}")
    print()

    if args.no_launch:
        print_next_commands(worktree, branch, prompt)
        return 0

    if reused and not args.new:
        resume_code = launch_codex_resume_last(worktree)
        if resume_code == 0:
            return 0
        print("No prior Codex session found for that worktree; starting a new one.", file=sys.stderr)

    return launch_codex_new(worktree, prompt)


def resolve_target_worktree(target: str | None) -> Path:
    if target:
        candidate = Path(target).expanduser()
        if candidate.exists():
            return candidate
        branch = branch_from_target(target)
        existing = worktree_for_branch(FORK_HOME, branch)
        if existing is not None:
            return existing
        raise SystemExit(fail(f"could not resolve worktree for target: {target}"))

    last = read_last_worktree()
    if last is None:
        raise SystemExit(
            fail("no last Codex fork worktree recorded yet; start one with `f codex-fork-task`")
        )
    return last


def cmd_last(args: argparse.Namespace) -> int:
    ensure_repo(FORK_HOME, "codex fork home checkout")
    worktree = resolve_target_worktree(args.target)
    if not worktree.exists():
        return fail(f"recorded worktree does not exist: {worktree}")
    write_last_worktree(worktree)
    return launch_codex_resume_last(worktree)


def review_branch_for(source_branch: str) -> str:
    if source_branch.startswith("review/"):
        return source_branch
    if source_branch.startswith("codex/"):
        suffix = source_branch.removeprefix("codex/").replace("/", "-")
        return f"{DEFAULT_REVIEW_PREFIX}-{suffix}"
    return f"{DEFAULT_REVIEW_PREFIX}-{source_branch.replace('/', '-')}"


def cmd_promote(args: argparse.Namespace) -> int:
    ensure_repo(FORK_HOME, "codex fork home checkout")
    target = resolve_target_worktree(args.target)
    ensure_repo(target, "codex fork worktree")
    source_branch = git_current_branch(target)
    review_branch = args.review_branch or review_branch_for(source_branch)

    source_commit = capture(["git", "rev-parse", "HEAD"], cwd=target)
    run(["git", "branch", "-f", review_branch, source_commit], cwd=FORK_HOME)
    print(f"source branch: {source_branch}")
    print(f"review branch: {review_branch}")
    print(f"commit:        {source_commit}")
    if args.push:
        run(["git", "push", "-u", DEFAULT_PRIVATE_REMOTE, review_branch], cwd=FORK_HOME)
        print(f"pushed to {DEFAULT_PRIVATE_REMOTE}/{review_branch}")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Automate the personal Codex fork home-branch/worktree workflow."
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    status = subparsers.add_parser("status", help="Show fork checkout/worktree state.")
    status.set_defaults(func=cmd_status)

    sync = subparsers.add_parser(
        "sync",
        help="Fast-forward nikiv in the personal fork checkout to upstream/main.",
    )
    sync.add_argument(
        "--push",
        action="store_true",
        help="Also push nikiv to the private remote after the fast-forward.",
    )
    sync.set_defaults(func=cmd_sync)

    task = subparsers.add_parser(
        "task",
        help="Create or reuse a scoped worktree for a Codex fork task and launch Codex there.",
    )
    task.add_argument("query", help="Natural-language task; used for branch slug + initial prompt.")
    task.add_argument(
        "--branch",
        help="Explicit branch name to use instead of deriving codex/<slug> from the query.",
    )
    task.add_argument(
        "--base",
        default=DEFAULT_BASE_BRANCH,
        help=f"Base branch/ref for new worktrees (default: {DEFAULT_BASE_BRANCH}).",
    )
    task.add_argument(
        "--path",
        help="Explicit worktree path to use instead of ~/.worktrees/codex/<branch>.",
    )
    task.add_argument(
        "--new",
        action="store_true",
        help="Always start a fresh Codex session instead of trying resume --last first.",
    )
    task.add_argument(
        "--no-launch",
        action="store_true",
        help="Only create/reuse the worktree and print the next command instead of launching Codex.",
    )
    task.set_defaults(func=cmd_task)

    last = subparsers.add_parser(
        "last",
        help="Resume the last Codex session in the last used fork worktree.",
    )
    last.add_argument(
        "target",
        nargs="?",
        help="Optional branch name or worktree path. Defaults to the last used fork worktree.",
    )
    last.set_defaults(func=cmd_last)

    promote = subparsers.add_parser(
        "promote",
        help="Create or update a review/nikiv-* branch from a codex/* worktree branch.",
    )
    promote.add_argument(
        "target",
        nargs="?",
        help="Optional branch name or worktree path. Defaults to the last used fork worktree.",
    )
    promote.add_argument(
        "--review-branch",
        help="Explicit review branch name instead of deriving review/nikiv-<slug>.",
    )
    promote.add_argument(
        "--push",
        action="store_true",
        help="Also push the promoted review branch to the private remote.",
    )
    promote.set_defaults(func=cmd_promote)

    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
