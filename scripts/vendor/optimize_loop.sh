#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

usage() {
  cat <<'USAGE'
Usage:
  scripts/vendor/optimize_loop.sh [--strict] [--no-bench] [--samples N] [--cmd "<command>"]

Examples:
  scripts/vendor/optimize_loop.sh
  scripts/vendor/optimize_loop.sh --strict --samples 2
  scripts/vendor/optimize_loop.sh --no-bench
USAGE
}

strict=false
no_bench=false
samples="${VENDOR_BENCH_SAMPLES:-2}"
bench_cmd="${VENDOR_BENCH_CMD:-cargo check -q}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --strict) strict=true; shift ;;
    --no-bench) no_bench=true; shift ;;
    --samples) samples="${2:-}"; shift 2 ;;
    --cmd) bench_cmd="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *)
      echo "error: unknown arg: $1"
      usage
      exit 1
      ;;
  esac
done

mkdir -p out/vendor

echo "== vendor rough-edge audit =="
if [[ "$strict" == true ]]; then
  python3 scripts/vendor/rough_edges_audit.py --strict-warnings | tee out/vendor/rough_edges_audit.txt
else
  python3 scripts/vendor/rough_edges_audit.py | tee out/vendor/rough_edges_audit.txt
fi

echo
echo "== offender scan =="
scripts/vendor/offenders.sh | tee out/vendor/offenders_latest.txt

if [[ "$no_bench" == false ]]; then
  echo
  echo "== iteration benchmark =="
  python3 scripts/vendor/bench_iteration.py --mode incremental --samples "$samples" --cmd "$bench_cmd"
fi

echo
echo "wrote:"
echo "  out/vendor/rough_edges_audit.txt"
echo "  out/vendor/offenders_latest.txt"
if [[ "$no_bench" == false ]]; then
  echo "  out/vendor/iteration_bench.jsonl"
fi
