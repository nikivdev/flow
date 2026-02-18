#!/usr/bin/env python3
import argparse
import json
import math
import os
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path
from typing import Dict, List, Tuple


def run_cmd(cmd: List[str], cwd: Path, env: Dict[str, str] | None = None, capture: bool = True) -> subprocess.CompletedProcess:
    merged_env = os.environ.copy()
    if env:
        merged_env.update(env)
    return subprocess.run(
        cmd,
        cwd=str(cwd),
        env=merged_env,
        text=True,
        capture_output=capture,
        check=False,
    )


def pct(values_us: List[float], p: float) -> float:
    if not values_us:
        return math.nan
    vals = sorted(values_us)
    idx = int(math.ceil((p / 100.0) * len(vals))) - 1
    idx = max(0, min(idx, len(vals) - 1))
    return vals[idx]


def summarize(values_us: List[float]) -> Dict[str, float]:
    return {
        "n": float(len(values_us)),
        "min_us": min(values_us),
        "p50_us": pct(values_us, 50),
        "p95_us": pct(values_us, 95),
        "p99_us": pct(values_us, 99),
        "mean_us": statistics.fmean(values_us),
        "max_us": max(values_us),
    }


def benchmark_command(
    *,
    label: str,
    cmd: List[str],
    cwd: Path,
    env: Dict[str, str] | None,
    warmup: int,
    iterations: int,
) -> Tuple[str, Dict[str, float]]:
    for _ in range(warmup):
        proc = run_cmd(cmd, cwd=cwd, env=env)
        if proc.returncode != 0:
            raise RuntimeError(f"warmup failed for {label}: {' '.join(cmd)}\n{proc.stderr}")

    durations_us: List[float] = []
    for _ in range(iterations):
        start = time.perf_counter_ns()
        proc = run_cmd(cmd, cwd=cwd, env=env)
        end = time.perf_counter_ns()
        if proc.returncode != 0:
            raise RuntimeError(f"run failed for {label}: {' '.join(cmd)}\n{proc.stderr}")
        durations_us.append((end - start) / 1000.0)

    return label, summarize(durations_us)


def find_flow_bin(repo: Path, flow_bin: str | None) -> str:
    if flow_bin:
        return flow_bin
    # Prefer debug binary first: during active refactors it's usually the freshest build.
    for candidate in [repo / "target" / "debug" / "f", repo / "target" / "release" / "f"]:
        if candidate.exists() and os.access(candidate, os.X_OK):
            return str(candidate)
    return "f"


def find_ai_taskd_client_bin(repo: Path) -> str | None:
    for candidate in [
        repo / "target" / "release" / "ai-taskd-client",
        repo / "target" / "debug" / "ai-taskd-client",
    ]:
        if candidate.exists() and os.access(candidate, os.X_OK):
            return str(candidate)
    return None


def ensure_cached_binary(repo: Path, flow_bin: str) -> str:
    proc = run_cmd([flow_bin, "tasks", "build-ai", "ai:flow/noop"], cwd=repo)
    if proc.returncode != 0:
        raise RuntimeError(f"failed to build noop task cache\n{proc.stderr}")

    match = re.search(r"binary:\s*(.+)", proc.stdout)
    if not match:
        raise RuntimeError(f"failed to parse cached binary path from output:\n{proc.stdout}")

    binary_path = match.group(1).strip()
    if not os.path.exists(binary_path):
        raise RuntimeError(f"cached binary path does not exist: {binary_path}")
    return binary_path


def main() -> int:
    parser = argparse.ArgumentParser(description="Benchmark Flow AI task runtime paths.")
    parser.add_argument("--iterations", type=int, default=50)
    parser.add_argument("--warmup", type=int, default=5)
    parser.add_argument("--flow-bin", default=None)
    parser.add_argument("--json-out", default="")
    args = parser.parse_args()

    if args.iterations <= 0:
        raise SystemExit("--iterations must be > 0")

    repo = Path(__file__).resolve().parents[1]
    flow_bin = find_flow_bin(repo, args.flow_bin)
    ai_taskd_client_bin = find_ai_taskd_client_bin(repo)

    print(f"repo: {repo}")
    print(f"flow_bin: {flow_bin}")
    if ai_taskd_client_bin:
        print(f"ai_taskd_client_bin: {ai_taskd_client_bin}")
    print(f"iterations={args.iterations} warmup={args.warmup}")

    # ensure daemon is up for daemon path benchmark
    _ = run_cmd([flow_bin, "tasks", "daemon", "start"], cwd=repo)

    cached_binary = ensure_cached_binary(repo, flow_bin)

    scenarios = [
        (
            "rust_help",
            [flow_bin, "--help"],
            None,
        ),
        (
            "moon_run_noop",
            [flow_bin, "ai:flow/noop"],
            {"FLOW_AI_TASK_RUNTIME": "moon-run"},
        ),
        (
            "cached_noop",
            [flow_bin, "ai:flow/noop"],
            {"FLOW_AI_TASK_RUNTIME": "cached"},
        ),
        (
            "daemon_cached_noop",
            [flow_bin, "tasks", "run-ai", "--daemon", "ai:flow/noop"],
            None,
        ),
        (
            "cached_binary_direct",
            [cached_binary],
            {"FLOW_AI_TASK_PROJECT_ROOT": str(repo)},
        ),
    ]
    if ai_taskd_client_bin:
        scenarios.append(
            (
                "daemon_client_noop",
                [ai_taskd_client_bin, "ai:flow/noop"],
                None,
            )
        )

    results: Dict[str, Dict[str, float]] = {}
    for label, cmd, env in scenarios:
        label, stats = benchmark_command(
            label=label,
            cmd=cmd,
            cwd=repo,
            env=env,
            warmup=args.warmup,
            iterations=args.iterations,
        )
        results[label] = stats
        print(
            f"{label:<22} n={int(stats['n'])} p50={stats['p50_us']:.1f}us "
            f"p95={stats['p95_us']:.1f}us p99={stats['p99_us']:.1f}us mean={stats['mean_us']:.1f}us"
        )

    cached_vs_moon = results["moon_run_noop"]["p95_us"] / results["cached_noop"]["p95_us"]
    daemon_vs_cached = results["daemon_cached_noop"]["p95_us"] / results["cached_noop"]["p95_us"]

    print(f"p95 ratio moon_run/cached: {cached_vs_moon:.2f}x")
    print(f"p95 ratio daemon/cached:  {daemon_vs_cached:.2f}x")
    daemon_client_vs_f = None
    if "daemon_client_noop" in results:
        daemon_client_vs_f = (
            results["daemon_cached_noop"]["p95_us"] / results["daemon_client_noop"]["p95_us"]
        )
        print(f"p95 ratio f-daemon/client-daemon:  {daemon_client_vs_f:.2f}x")

    payload = {
        "repo": str(repo),
        "flow_bin": flow_bin,
        "iterations": args.iterations,
        "warmup": args.warmup,
        "results": results,
        "ratios": {
            "moon_run_p95_div_cached_p95": cached_vs_moon,
            "daemon_p95_div_cached_p95": daemon_vs_cached,
            "f_daemon_p95_div_client_daemon_p95": daemon_client_vs_f,
        },
    }

    if args.json_out:
        out = Path(args.json_out)
        out.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        print(f"wrote: {out}")

    _ = run_cmd([flow_bin, "tasks", "daemon", "stop"], cwd=repo)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
