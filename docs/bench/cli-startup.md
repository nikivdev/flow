# Flow CLI Startup Benchmark

Use this to measure cold-ish user-facing Flow latency on cheap commands that should stay fast.

## Run

```bash
f bench-cli-startup
```

Useful knobs:

```bash
f bench-cli-startup -- --iterations 30 --warmup 5
f bench-cli-startup -- --flow-bin ./target/release/f
f bench-cli-startup -- --json-out out/bench/cli-startup.json
```

## What it measures

- `f --help`
- `f --help-full`
- `f info`
- `f projects`
- `f analytics status`
- `f tasks list`
- `f tasks dupes`

The script forces:

- `CI=1`
- `FLOW_ANALYTICS_DISABLE=1`

That keeps the benchmark focused on Flow startup and command dispatch instead of analytics prompts.

## Readouts to track

- `p50_ms`
- `p95_ms`
- `p99_ms`
- `mean_ms`

For startup work, keep the policy simple:

- optimize only if median improves
- reject changes that materially regress `p95`
- benchmark from the same binary and same repo fixture

## Why these commands

These commands are the best early signal for startup overhead because they are local, cheap, non-interactive, and sensitive to unnecessary pre-dispatch work such as eager secrets loading and auto skill sync.
