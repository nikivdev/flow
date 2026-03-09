#!/usr/bin/env python3
from __future__ import annotations

import os
import pathlib
import subprocess
import sys


def main() -> int:
    root = pathlib.Path(__file__).resolve().parent.parent
    output = root / "src" / "help_full.json"
    env = os.environ.copy()
    env["FLOW_REGENERATE_HELP_FULL"] = "1"
    cmd = ["cargo", "run", "--quiet", "--bin", "f", "--", "--help-full"]
    result = subprocess.run(cmd, cwd=root, env=env, capture_output=True, text=True)
    if result.returncode != 0:
        sys.stderr.write(result.stderr)
        return result.returncode
    output.write_text(result.stdout, encoding="utf-8")
    print(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
