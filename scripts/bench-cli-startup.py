#!/usr/bin/env python3
import argparse
import json
import math
import os
import statistics
import subprocess
import time
from pathlib import Path
from typing import Dict, List, Tuple


def pct(values_ms: List[float], p: float) -> float:
    if not values_ms:
        return math.nan
    vals = sorted(values_ms)
    idx = int(math.ceil((p / 100.0) * len(vals))) - 1
    idx = max(0, min(idx, len(vals) - 1))
    return vals[idx]


def summarize(values_ms: List[float]) -> Dict[str, float]:
    return {
        "n": float(len(values_ms)),
        "min_ms": min(values_ms),
        "p50_ms": pct(values_ms, 50),
        "p95_ms": pct(values_ms, 95),
        "p99_ms": pct(values_ms, 99),
        "mean_ms": statistics.fmean(values_ms),
        "max_ms": max(values_ms),
    }


def run_cmd(
    cmd: List[str],
    *,
    cwd: Path,
    env: Dict[str, str],
) -> subprocess.CompletedProcess[str]:
    merged_env = os.environ.copy()
    merged_env.update(env)
    return subprocess.run(
        cmd,
        cwd=str(cwd),
        env=merged_env,
        text=True,
        capture_output=True,
        check=False,
    )


def benchmark_command(
    *,
    label: str,
    cmd: List[str],
    cwd: Path,
    env: Dict[str, str],
    warmup: int,
    iterations: int,
) -> Tuple[str, Dict[str, float]]:
    for _ in range(warmup):
        proc = run_cmd(cmd, cwd=cwd, env=env)
        if proc.returncode != 0:
            raise RuntimeError(
                f"warmup failed for {label}: {' '.join(cmd)}\n"
                f"stdout:\n{proc.stdout}\n"
                f"stderr:\n{proc.stderr}"
            )

    durations_ms: List[float] = []
    for _ in range(iterations):
        start = time.perf_counter_ns()
        proc = run_cmd(cmd, cwd=cwd, env=env)
        end = time.perf_counter_ns()
        if proc.returncode != 0:
            raise RuntimeError(
                f"run failed for {label}: {' '.join(cmd)}\n"
                f"stdout:\n{proc.stdout}\n"
                f"stderr:\n{proc.stderr}"
            )
        durations_ms.append((end - start) / 1_000_000.0)

    return label, summarize(durations_ms)


def find_flow_bin(repo: Path, flow_bin: str | None) -> str:
    if flow_bin:
        return flow_bin
    for candidate in [repo / "target" / "release" / "f", repo / "target" / "debug" / "f"]:
        if candidate.exists() and os.access(candidate, os.X_OK):
            return str(candidate)
    return "f"


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Benchmark Flow CLI startup and low-latency read-only commands."
    )
    parser.add_argument("--iterations", type=int, default=20)
    parser.add_argument("--warmup", type=int, default=3)
    parser.add_argument("--flow-bin", default=None)
    parser.add_argument("--project-root", default=".")
    parser.add_argument("--json-out", default="")
    args = parser.parse_args()

    if args.iterations <= 0:
        raise SystemExit("--iterations must be > 0")
    if args.warmup < 0:
        raise SystemExit("--warmup must be >= 0")

    repo = Path(__file__).resolve().parents[1]
    project_root = Path(args.project_root).expanduser()
    if not project_root.is_absolute():
        project_root = (repo / project_root).resolve()
    flow_bin = find_flow_bin(repo, args.flow_bin)

    base_env = {
        "CI": "1",
        "FLOW_ANALYTICS_DISABLE": "1",
    }

    scenarios = [
        ("help", [flow_bin, "--help"], repo),
        ("help_full", [flow_bin, "--help-full"], repo),
        ("info", [flow_bin, "info"], project_root),
        ("projects", [flow_bin, "projects"], project_root),
        ("analytics_status", [flow_bin, "analytics", "status"], project_root),
        ("tasks_list", [flow_bin, "tasks", "list"], project_root),
        ("tasks_dupes", [flow_bin, "tasks", "dupes"], project_root),
        ("deploy_show_host", [flow_bin, "deploy", "show-host"], project_root),
    ]

    print(f"repo: {repo}", flush=True)
    print(f"project_root: {project_root}", flush=True)
    print(f"flow_bin: {flow_bin}", flush=True)
    print(f"iterations={args.iterations} warmup={args.warmup}", flush=True)

    results: Dict[str, Dict[str, float]] = {}
    for label, cmd, cwd in scenarios:
        label, stats = benchmark_command(
            label=label,
            cmd=cmd,
            cwd=cwd,
            env=base_env,
            warmup=args.warmup,
            iterations=args.iterations,
        )
        results[label] = stats
        print(
            f"{label:<18} n={int(stats['n'])} p50={stats['p50_ms']:.2f}ms "
            f"p95={stats['p95_ms']:.2f}ms p99={stats['p99_ms']:.2f}ms "
            f"mean={stats['mean_ms']:.2f}ms",
            flush=True,
        )

    payload = {
        "repo": str(repo),
        "project_root": str(project_root),
        "flow_bin": flow_bin,
        "iterations": args.iterations,
        "warmup": args.warmup,
        "results": results,
    }

    if args.json_out:
        out = Path(args.json_out).expanduser()
        if not out.is_absolute():
            out = (repo / out).resolve()
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        print(f"wrote: {out}", flush=True)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
