#!/usr/bin/env python3
import argparse
import json
import os
import re
import subprocess
from pathlib import Path
from typing import Dict


def run(cmd, cwd: Path, env: Dict[str, str] | None = None) -> subprocess.CompletedProcess:
    merged = os.environ.copy()
    if env:
        merged.update(env)
    return subprocess.run(cmd, cwd=str(cwd), text=True, capture_output=True, env=merged, check=False)


def parse_metrics(text: str) -> Dict[str, Dict[str, float]]:
    metrics: Dict[str, Dict[str, float]] = {}
    pattern = re.compile(r"^(\S+)\s+ns_total=(\d+)\s+ns_per_op=([0-9.]+)\s+checksum=(\d+)$")
    for line in text.splitlines():
        m = pattern.match(line.strip())
        if not m:
            continue
        label = m.group(1)
        total = int(m.group(2))
        per_op = float(m.group(3))
        checksum = int(m.group(4))
        metrics[label] = {
            "ns_total": float(total),
            "ns_per_op_reported": per_op,
            "checksum": float(checksum),
        }
    return metrics


def write_moon_pkg(moon_dir: Path, rust_lib_dir: Path, cc_flags: str) -> None:
    template = (moon_dir / "moon.pkg.template.json").read_text(encoding="utf-8")
    flags = f"-L{rust_lib_dir} -lflow_ffi_host_boundary"
    body = template.replace("__CC_FLAGS__", cc_flags).replace("__CC_LINK_FLAGS__", flags)
    (moon_dir / "moon.pkg.json").write_text(body, encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(description="Benchmark MoonBit <-> Rust FFI boundary overhead.")
    parser.add_argument("--iters", type=int, default=10_000_000)
    parser.add_argument(
        "--native-opt",
        action="store_true",
        help="Enable machine-local tuning (Rust target-cpu=native and Moon cc-flags -O3 -march=native).",
    )
    parser.add_argument("--json-out", default="")
    args = parser.parse_args()

    if args.iters <= 0:
        raise SystemExit("--iters must be > 0")

    root = Path(__file__).resolve().parents[1]
    rust_manifest = root / "bench" / "ffi_host_boundary" / "Cargo.toml"
    rust_dir = rust_manifest.parent
    moon_dir = root / "bench" / "moon_ffi_boundary"
    rust_lib_dir = rust_dir / "target" / "release"

    env = {"FLOW_FFI_ITERS": str(args.iters)}
    moon_cc_flags = "-O3"
    if args.native_opt:
        env["RUSTFLAGS"] = "-C target-cpu=native"
        moon_cc_flags = "-O3 -march=native -mtune=native"

    print(f"root: {root}")
    print(f"iters: {args.iters}")

    build = run([
        "cargo",
        "build",
        "--manifest-path",
        str(rust_manifest),
        "--release",
    ], cwd=root)
    if build.returncode != 0:
        print(build.stdout)
        print(build.stderr)
        raise SystemExit("failed to build rust ffi host crate")

    write_moon_pkg(moon_dir, rust_lib_dir, moon_cc_flags)

    rust_proc = run([
        "cargo",
        "run",
        "--manifest-path",
        str(rust_manifest),
        "--release",
        "--bin",
        "rust_boundary_bench",
        "--",
        "--iters",
        str(args.iters),
    ], cwd=root, env=env)
    if rust_proc.returncode != 0:
        print(rust_proc.stdout)
        print(rust_proc.stderr)
        raise SystemExit("rust benchmark failed")

    moon_proc = run([
        "moon",
        "-C",
        str(moon_dir),
        "run",
        "main.mbt",
        "--target",
        "native",
        "--release",
    ], cwd=root, env=env)
    if moon_proc.returncode != 0:
        print(moon_proc.stdout)
        print(moon_proc.stderr)
        raise SystemExit("moon benchmark failed")

    rust_metrics = parse_metrics(rust_proc.stdout)
    moon_metrics = parse_metrics(moon_proc.stdout)

    required = [
        "rust_inline_add",
        "rust_fn_add",
        "rust_extern_add",
        "rust_extern_noop",
        "moon_ffi_add",
        "moon_ffi_noop",
    ]
    missing = [key for key in required if key not in rust_metrics and key not in moon_metrics]
    if missing:
        raise SystemExit(f"missing metrics in output: {missing}")

    def ns_per_op(metrics: Dict[str, Dict[str, float]], key: str) -> float:
        return metrics[key]["ns_total"] / float(args.iters)

    print("--- Rust ---")
    for key in ["rust_inline_add", "rust_fn_add", "rust_extern_add", "rust_extern_noop"]:
        if key in rust_metrics:
            m = rust_metrics[key]
            print(
                f"{key:<18} ns/op={ns_per_op(rust_metrics, key):.4f} "
                f"total_ns={int(m['ns_total'])} checksum={int(m['checksum'])}"
            )

    print("--- MoonBit ---")
    for key in ["moon_add", "moon_ffi_add", "moon_ffi_noop"]:
        if key in moon_metrics:
            m = moon_metrics[key]
            print(
                f"{key:<18} ns/op={ns_per_op(moon_metrics, key):.4f} "
                f"total_ns={int(m['ns_total'])} checksum={int(m['checksum'])}"
            )

    ratios = {
        "moon_ffi_add_div_rust_extern_add": ns_per_op(moon_metrics, "moon_ffi_add")
        / ns_per_op(rust_metrics, "rust_extern_add"),
        "moon_ffi_noop_div_rust_extern_noop": ns_per_op(moon_metrics, "moon_ffi_noop")
        / ns_per_op(rust_metrics, "rust_extern_noop"),
    }

    print("--- Ratios ---")
    for k, v in ratios.items():
        print(f"{k}: {v:.3f}x")

    payload = {
        "iters": args.iters,
        "native_opt": args.native_opt,
        "rust": rust_metrics,
        "moon": moon_metrics,
        "ns_per_op": {
            "rust_inline_add": ns_per_op(rust_metrics, "rust_inline_add"),
            "rust_fn_add": ns_per_op(rust_metrics, "rust_fn_add"),
            "rust_extern_add": ns_per_op(rust_metrics, "rust_extern_add"),
            "rust_extern_noop": ns_per_op(rust_metrics, "rust_extern_noop"),
            "moon_ffi_add": ns_per_op(moon_metrics, "moon_ffi_add"),
            "moon_ffi_noop": ns_per_op(moon_metrics, "moon_ffi_noop"),
        },
        "ratios": ratios,
    }
    if "moon_add" in moon_metrics:
        payload["ns_per_op"]["moon_add"] = ns_per_op(moon_metrics, "moon_add")

    if args.json_out:
        out = Path(args.json_out)
        out.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        print(f"wrote: {out}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
