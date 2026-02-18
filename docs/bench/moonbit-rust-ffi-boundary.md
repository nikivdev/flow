# MoonBit <-> Rust FFI Boundary Benchmark

This benchmark measures raw call overhead between MoonBit (native C backend) and Rust host functions.

## Scope

Measured:
- Rust local math baseline (`rust_inline_add`, `rust_fn_add`)
- Rust calling exported C ABI functions (`rust_extern_add`, `rust_extern_noop`)
- MoonBit calling Rust-exported C ABI (`moon_ffi_add`, `moon_ffi_noop`)

Not measured:
- app-level task execution
- process startup/teardown
- end-to-end Flow command latency

## Where the code lives

- `bench/ffi_host_boundary/src/lib.rs`
- `bench/ffi_host_boundary/src/bin/rust_boundary_bench.rs`
- `bench/moon_ffi_boundary/main.mbt`
- `bench/moon_ffi_boundary/moon.pkg.template.json`
- `scripts/bench-moonbit-rust-ffi.py`
- Flow task: `bench-ffi-boundary` in `flow.toml`

## Run commands

Direct script:

```bash
cd ~/code/flow
python3 scripts/bench-moonbit-rust-ffi.py --iters 10000000 --json-out /tmp/ffi.json
```

With machine-local tuning flags:

```bash
cd ~/code/flow
python3 scripts/bench-moonbit-rust-ffi.py --iters 10000000 --native-opt --json-out /tmp/ffi_native.json
```

Via Flow:

```bash
cd ~/code/flow
f bench-ffi-boundary --iters 10000000 --json-out /tmp/ffi_flow.json
```

## Latest measured numbers (this machine)

Method: 3 rounds each, 10M iterations/round, median ns/op.

| metric | baseline | native-opt | tuned/base |
|---|---:|---:|---:|
| rust_extern_add | 2.7683 | 2.8291 | 1.022x |
| rust_extern_noop | 2.7880 | 2.9140 | 1.045x |
| moon_ffi_add | 0.9005 | 1.1075 | 1.230x |
| moon_ffi_noop | 0.8576 | 0.8462 | 0.987x |

Interpretation:
- Boundary overhead is single-digit nanoseconds.
- On this machine, `--native-opt` did not consistently improve results; it was mixed/slightly worse for most metrics.
- You must benchmark on your target machine before locking optimization flags.

## Important measurement caveat

`moon_add` (pure MoonBit loop) can be optimized away by the compiler and report `0 ns`. Treat it as non-authoritative for boundary decisions.

Use FFI metrics (`moon_ffi_*`, `rust_extern_*`) as the primary signal.

## Optimizations implemented

1. Exported host functions stripped to minimal arithmetic (removed internal `black_box`).
2. Rust FFI functions marked `#[inline(never)]` to avoid accidental inlining in host-side tests.
3. Rust bench crate release profile tightened:
   - `lto = "fat"`
   - `codegen-units = 1`
   - `panic = "abort"`
   - `strip = true`
4. Moon native flags are configurable via template placeholder:
   - `cc-flags` in `moon.pkg.template.json`
5. Benchmark script supports `--native-opt`:
   - Rust: `RUSTFLAGS=-C target-cpu=native`
   - Moon native: `-O3 -march=native -mtune=native`

## Ideas borrowed from Moon toolchain

From `~/repos/moonbitlang/moon`:
- `Cargo.toml` uses explicit release profile controls (`lto`, `codegen-units`, `strip`) for predictable build/runtime tradeoffs.
- Native pipeline exposes per-package `cc-flags` / `cc-link-flags` (schema in `crates/moonbuild/template/pkg.schema.json`).
- TCC-run mode exists to speed dev-time run loops, not runtime performance; custom native flags disable that fast dev path.

Practical implication:
- For runtime benchmarks, tune native flags carefully and measure.
- For developer iteration speed, avoid unnecessary custom flags when tcc-run is beneficial.

## Decision guidance: should Rust core move to MoonBit for speed?

Based on these numbers: no, not for boundary speed alone.

Reason:
- Boundary is already near-zero cost (sub-3ns on this host).
- Any wins from migration will mostly come from architecture choices (fewer crossings, coarser APIs), not language-switch micro-optimizations.

Move code to MoonBit for:
- faster iteration/modeling
- portability or generation benefits
- maintainability

Keep in Rust when:
- syscall-heavy paths
- mature unsafe/perf-critical internals already stable

## Next benchmark to add (recommended)

Add a coarse-grained benchmark for real task payload crossing:
- one boundary call carrying a packed command
- compare against N tiny calls
- report payload sizes and p50/p95

That will better predict real Flow task runtime behavior than arithmetic no-op microbenchmarks.
