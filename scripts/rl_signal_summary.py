#!/usr/bin/env python3
"""Summarize flow RL signal JSONL output."""

from __future__ import annotations

import argparse
import json
from collections import Counter, defaultdict
from pathlib import Path


def percentile(sorted_values: list[int], pct: float) -> int:
    if not sorted_values:
        return 0
    idx = int((len(sorted_values) - 1) * pct)
    return sorted_values[max(0, min(idx, len(sorted_values) - 1))]


def main() -> int:
    parser = argparse.ArgumentParser(description="Summarize flow RL signal JSONL")
    parser.add_argument(
        "path",
        nargs="?",
        default="out/logs/flow_rl_signals.jsonl",
        help="Path to flow RL signal JSONL",
    )
    parser.add_argument("--last", type=int, default=0, help="Only process last N lines")
    args = parser.parse_args()

    path = Path(args.path).expanduser().resolve()
    if not path.exists():
        print(f"missing file: {path}")
        return 1

    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    if args.last > 0:
        lines = lines[-args.last :]

    total = 0
    by_event = Counter()
    by_error = Counter()
    durations: defaultdict[str, list[int]] = defaultdict(list)

    for raw in lines:
        raw = raw.strip()
        if not raw:
            continue
        try:
            row = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if not isinstance(row, dict):
            continue

        total += 1
        event = str(row.get("event_type", "unknown"))
        by_event[event] += 1

        err_cls = row.get("error_class")
        if err_cls:
            by_error[str(err_cls)] += 1

        dur = row.get("duration_ms")
        if isinstance(dur, int) and dur >= 0:
            durations[event].append(dur)

    print(f"file: {path}")
    print(f"rows: {total}")
    print("")
    print("event counts:")
    for event, count in by_event.most_common():
        print(f"  {event}: {count}")

    if by_error:
        print("")
        print("error classes:")
        for err, count in by_error.most_common():
            print(f"  {err}: {count}")

    if durations:
        print("")
        print("duration ms:")
        for event, values in sorted(durations.items()):
            values.sort()
            p50 = percentile(values, 0.50)
            p95 = percentile(values, 0.95)
            p99 = percentile(values, 0.99)
            print(f"  {event}: p50={p50} p95={p95} p99={p99} n={len(values)}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())

