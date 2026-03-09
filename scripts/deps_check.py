#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


def run_json(cmd: list[str], *, cwd: Path) -> Any:
    result = subprocess.run(
        cmd,
        cwd=cwd,
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"command failed: {' '.join(cmd)}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return json.loads(result.stdout)


def fetch_latest(crate: str, cache: dict[str, str]) -> str:
    if crate in cache:
        return cache[crate]
    url = f"https://crates.io/api/v1/crates/{crate}"
    request = urllib.request.Request(
        url,
        headers={
            "Accept": "application/json",
            "User-Agent": "flow-deps-check/1.0",
        },
    )
    last_error: Exception | None = None
    for attempt in range(3):
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                payload = json.load(response)
            break
        except (TimeoutError, urllib.error.URLError) as error:
            last_error = error
            if attempt == 2:
                raise RuntimeError(f"failed to fetch latest version for {crate}: {error}") from error
            time.sleep(1.0 + attempt)
    else:
        raise RuntimeError(f"failed to fetch latest version for {crate}: {last_error}")
    latest = payload["crate"].get("max_stable_version") or payload["crate"]["newest_version"]
    cache[crate] = latest
    return latest


def load_vendor_rows(repo_root: Path) -> list[dict[str, Any]]:
    rows = run_json(["scripts/vendor/check-upstream.sh", "--json"], cwd=repo_root)
    rows.sort(key=lambda row: row["crate"])
    return rows


def load_direct_rows(repo_root: Path) -> list[dict[str, Any]]:
    metadata = run_json(["cargo", "metadata", "--format-version", "1", "--locked"], cwd=repo_root)
    packages_by_id = {pkg["id"]: pkg for pkg in metadata["packages"]}
    nodes_by_id = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    workspace_member_ids = set(metadata.get("workspace_members", []))
    latest_cache: dict[str, str] = {}
    rows: list[dict[str, Any]] = []
    seen_rows: set[tuple[str, str, tuple[str, ...], str]] = set()

    for member_id in sorted(workspace_member_ids):
        workspace_pkg = packages_by_id[member_id]
        workspace_name = workspace_pkg["name"]
        workspace_node = nodes_by_id[member_id]

        for dep in workspace_node.get("deps", []):
            pkg_id = dep["pkg"]
            pkg = packages_by_id[pkg_id]
            if pkg.get("source") is None:
                continue

            kinds = tuple(
                sorted(
                    {
                        dep_kind.get("kind") or "normal"
                        for dep_kind in dep.get("dep_kinds", [])
                    }
                )
            ) or ("normal",)
            row_key = (workspace_name, pkg["name"], kinds, pkg["version"])
            if row_key in seen_rows:
                continue
            seen_rows.add(row_key)

            current = pkg["version"]
            latest = fetch_latest(pkg["name"], latest_cache)
            rows.append(
                {
                    "workspace": workspace_name,
                    "crate": pkg["name"],
                    "current": current,
                    "latest": latest,
                    "kinds": list(kinds),
                    "status": "up-to-date" if current == latest else "update-available",
                }
            )

    rows.sort(key=lambda row: (row["workspace"], row["crate"]))
    return rows


def print_rows(title: str, rows: list[dict[str, Any]], *, include_kinds: bool) -> None:
    print(title)
    if not rows:
        print("  none")
        return
    for row in rows:
        suffix = ""
        if include_kinds:
            suffix = f" [{row['workspace']}] ({','.join(row['kinds'])})"
        print(
            f"  {row['crate']}{suffix}: current={row['current']} latest={row['latest']} status={row['status']}"
        )


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check Flow vendored and direct Cargo dependencies against the latest upstream releases."
    )
    parser.add_argument("--json", action="store_true", help="Emit machine-readable JSON")
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parent.parent
    vendor_rows = load_vendor_rows(repo_root)
    direct_rows = load_direct_rows(repo_root)

    stale_vendor = [row for row in vendor_rows if row["status"] != "up-to-date"]
    stale_direct = [row for row in direct_rows if row["status"] != "up-to-date"]

    payload = {
        "vendor": vendor_rows,
        "direct": direct_rows,
        "ok": not stale_vendor and not stale_direct,
    }

    if args.json:
        print(json.dumps(payload, indent=2))
    else:
        print_rows("Vendored deps", vendor_rows, include_kinds=False)
        print_rows("Direct Cargo deps", direct_rows, include_kinds=True)
        print()
        if payload["ok"]:
            print("deps-check: ok")
        else:
            print(
                f"deps-check: failed ({len(stale_vendor)} vendored stale, {len(stale_direct)} direct stale)"
            )

    return 0 if payload["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
