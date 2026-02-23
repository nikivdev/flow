# Vendor Optimization Loop

This is the practical loop for aggressively optimizing dependencies in `flow`
while keeping Cargo correctness and upstream sync reliability.

## Goals

- find and fix vendoring rough edges early,
- rank high-impact dependency offenders,
- track compile-iteration speed improvements over time.

## Commands

```bash
f vendor-rough-audit
f vendor-offenders
f vendor-bench-iter -- --mode incremental --samples 3
```

One-command loop:

```bash
f vendor-optimize-loop
```

Strict mode (warnings fail):

```bash
f vendor-optimize-loop -- --strict
```

## What Each Tool Checks

`vendor-rough-audit` (`scripts/vendor/rough_edges_audit.py`) checks:

- lock/manifests/materialized crate path consistency,
- Cargo patch wiring vs `vendor.lock.toml`,
- vendored crate resolution in `Cargo.lock` (no registry source),
- provenance fields in manifests (`history_head`, `upstream_repository`),
- stale code index detection (`.vendor/typesense/sources.json` freshness),
- extra drift artifacts (`lib/vendor/*` or patch entries not in lock).

`vendor-offenders` (`scripts/vendor/offenders.sh`) shows:

- direct dependencies ranked by transitive tree size,
- duplicate version pressure (`cargo tree -d`),
- proc-macro footprint.

`vendor-bench-iter` (`scripts/vendor/bench_iteration.py`) provides:

- repeated timing samples for a compile command (default `cargo check -q`),
- rolling comparison against prior runs from `out/vendor/iteration_bench.jsonl`,
- optional fail threshold for gating regressions.

## Output Artifacts

- `out/vendor/rough_edges_audit.txt`
- `out/vendor/offenders_latest.txt`
- `out/vendor/iteration_bench.jsonl`

These files make optimization work reviewable and repeatable across sessions.

## Suggested Weekly Cadence

1. `f vendor-optimize-loop -- --strict --samples 2`
2. Pick top 1-2 offender crates.
3. Apply trim/rewrite changes.
4. Re-run loop.
5. Confirm no new rough-edge findings.
6. Confirm compile iteration trend improves or stays flat.
7. Confirm upstream sync remains clean (`scripts/vendor/sync-all.sh --important --dry-run`).
