#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Fail when CLI startup benchmark results exceed repository thresholds."
    )
    parser.add_argument("benchmark_json", help="Path to JSON output from scripts/bench-cli-startup.py")
    parser.add_argument(
        "--thresholds",
        default=str(Path(__file__).with_name("cli_startup_thresholds.json")),
        help="Path to threshold JSON file",
    )
    args = parser.parse_args()

    benchmark_path = Path(args.benchmark_json).expanduser()
    thresholds_path = Path(args.thresholds).expanduser()

    payload = json.loads(benchmark_path.read_text(encoding="utf-8"))
    thresholds = json.loads(thresholds_path.read_text(encoding="utf-8"))

    violations: list[str] = []
    results = payload.get("results", {})

    for scenario, expected in thresholds.items():
        actual = results.get(scenario)
        if actual is None:
            violations.append(f"{scenario}: missing from benchmark output")
            continue
        for metric, limit in expected.items():
            value = actual.get(metric)
            if value is None:
                violations.append(f"{scenario}: missing metric {metric}")
                continue
            if value > limit:
                violations.append(
                    f"{scenario}: {metric}={value:.2f}ms exceeds {limit:.2f}ms"
                )

    if violations:
        print("CLI startup threshold violations:")
        for violation in violations:
            print(f"  - {violation}")
        return 1

    print("CLI startup thresholds passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
