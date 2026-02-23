#!/usr/bin/env python3
"""Record and compare compile-iteration timings for vendoring work."""

from __future__ import annotations

import argparse
import json
import subprocess
import time
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Literal


Mode = Literal["incremental", "clean"]


@dataclass
class BenchRow:
    timestamp_utc: str
    project: str
    git_commit: str
    mode: Mode
    sample: int
    seconds: float
    command: str


def utc_now() -> str:
    return datetime.now(tz=timezone.utc).isoformat()


def git_head(project: Path) -> str:
    try:
        out = subprocess.check_output(
            ["git", "-C", str(project), "rev-parse", "--short", "HEAD"],
            text=True,
        ).strip()
        return out or "unknown"
    except Exception:
        return "unknown"


def read_jsonl(path: Path) -> list[dict]:
    if not path.exists():
        return []
    rows: list[dict] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return rows


def append_jsonl(path: Path, rows: list[BenchRow]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as fh:
        for row in rows:
            fh.write(json.dumps(asdict(row), ensure_ascii=False) + "\n")


def run_cmd(project: Path, cmd: str) -> float:
    start = time.perf_counter()
    proc = subprocess.run(cmd, shell=True, cwd=project)
    end = time.perf_counter()
    if proc.returncode != 0:
        raise RuntimeError(f"command failed ({proc.returncode}): {cmd}")
    return end - start


def run_sample(project: Path, mode: Mode, cmd: str) -> float:
    if mode == "clean":
        run_cmd(project, "cargo clean")
    return run_cmd(project, cmd)


def summarize(values: list[float]) -> dict[str, float]:
    if not values:
        return {"min": 0.0, "avg": 0.0, "max": 0.0}
    return {
        "min": min(values),
        "avg": sum(values) / len(values),
        "max": max(values),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description="Benchmark vendoring iteration speed")
    parser.add_argument("--project", default=".", help="Project root (default: .)")
    parser.add_argument("--mode", choices=["incremental", "clean", "both"], default="incremental")
    parser.add_argument("--samples", type=int, default=3, help="Samples per mode")
    parser.add_argument("--cmd", default="cargo check -q", help="Command to benchmark")
    parser.add_argument("--record", default="out/vendor/iteration_bench.jsonl", help="JSONL output path")
    parser.add_argument("--compare-window", type=int, default=10, help="Prior rows to compare against")
    parser.add_argument("--fail-above", type=float, default=0.0, help="Fail if avg seconds exceeds threshold")
    args = parser.parse_args()

    project = Path(args.project).expanduser().resolve()
    record_path = project / args.record

    modes: list[Mode]
    if args.mode == "both":
        modes = ["clean", "incremental"]
    else:
        modes = [args.mode]

    prior = read_jsonl(record_path)
    git_commit = git_head(project)
    all_rows: list[BenchRow] = []

    for mode in modes:
        print(f"mode: {mode}")
        values: list[float] = []
        for i in range(1, args.samples + 1):
            secs = run_sample(project, mode, args.cmd)
            values.append(secs)
            row = BenchRow(
                timestamp_utc=utc_now(),
                project=str(project),
                git_commit=git_commit,
                mode=mode,
                sample=i,
                seconds=secs,
                command=args.cmd,
            )
            all_rows.append(row)
            print(f"  sample {i}/{args.samples}: {secs:.3f}s")

        stats = summarize(values)
        print(f"  min/avg/max: {stats['min']:.3f}s / {stats['avg']:.3f}s / {stats['max']:.3f}s")

        prev_values = [
            float(row.get("seconds", 0.0))
            for row in prior
            if row.get("mode") == mode and row.get("command") == args.cmd
        ]
        if prev_values:
            window = prev_values[-args.compare_window :]
            prev_avg = sum(window) / len(window)
            delta = stats["avg"] - prev_avg
            direction = "+" if delta >= 0 else "-"
            print(f"  delta vs last {len(window)} avg: {direction}{abs(delta):.3f}s (prev {prev_avg:.3f}s)")

        if args.fail_above > 0 and stats["avg"] > args.fail_above:
            append_jsonl(record_path, all_rows)
            raise SystemExit(
                f"avg {mode} time {stats['avg']:.3f}s exceeds fail-above {args.fail_above:.3f}s"
            )

    append_jsonl(record_path, all_rows)
    print(f"recorded: {len(all_rows)} samples -> {record_path}")


if __name__ == "__main__":
    main()
