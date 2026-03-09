#!/usr/bin/env python3
from __future__ import annotations

import pathlib
import re
import sys


def read_package_version(cargo_toml: pathlib.Path) -> str:
    text = cargo_toml.read_text(encoding="utf-8")
    in_package = False
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if line.startswith("["):
            in_package = line == "[package]"
            continue
        if not in_package:
            continue
        match = re.match(r'version\s*=\s*"([^"]+)"', line)
        if match:
            return match.group(1)
    raise RuntimeError(f"failed to find [package].version in {cargo_toml}")


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: check_release_tag_version.py <tag>", file=sys.stderr)
        return 2

    tag = argv[1]
    if not tag.startswith("v"):
        print(f"error: expected release tag like vX.Y.Z, got {tag}", file=sys.stderr)
        return 1

    root = pathlib.Path(__file__).resolve().parent.parent
    version = read_package_version(root / "Cargo.toml")
    expected_tag = f"v{version}"
    if tag != expected_tag:
        print(
            f"error: release tag {tag} does not match Cargo.toml version {version} "
            f"(expected {expected_tag})",
            file=sys.stderr,
        )
        return 1

    print(f"verified: release tag {tag} matches Cargo.toml version {version}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
