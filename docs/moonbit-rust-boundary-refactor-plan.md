# MoonBit Runtime Refactor Plan (Flow)

This plan is based on the current implementation in:

- `src/ai_tasks.rs`
- `src/ai_taskd.rs`
- `src/tasks.rs`
- `.ai/tasks/flow/*`

Goal: keep Flow's Rust core stable while moving high-change task logic to MoonBit with near-zero boundary overhead and no performance regressions.

## 1. Current Scan: Where Refactor Pressure Exists

### 1.1 Task execution policy is still split across multiple layers

Current paths:

- `f ai:...` and task shortcut route through `tasks::run_with_discovery` in `src/tasks.rs`.
- runtime policy (cached vs moon-run fallback) lives in `ai_tasks::run_task` in `src/ai_tasks.rs`.
- daemon path (`f tasks run-ai --daemon`) is implemented separately in `src/ai_taskd.rs`.

Refactor target:

- Introduce one `AiTaskExecutor` policy entrypoint in Rust, used by all callsites.
- Make shortcut path and explicit `tasks run-ai` path share identical behavior and telemetry.

### 1.2 Startup/daemon policy is command-level, not config-level

Current state:

- daemon usage is chosen by CLI flags.

Refactor target:

- Add config-level defaults (e.g. `[ai_tasks] mode = "cached"`, `daemon = true`) and keep CLI as override.

### 1.3 Task pack design is still shell-heavy

Current state:

- most `.ai/tasks/flow/*` tasks call shell commands directly.

Refactor target:

- move hot utility operations to typed host APIs over a stable ABI (git, file, json, process spawn, clock), reducing shell parsing/process overhead and improving deterministic latency.

## 2. Benchmark Harness (Implemented)

Added:

- `scripts/bench-ai-runtime.py`
- `scripts/bench-moonbit-rust-ffi.py`
- `flow.toml` task: `bench-ai-runtime`
- `flow.toml` task: `bench-ffi-boundary`
- minimal benchmark task: `.ai/tasks/flow/noop/*`
- FFI microbench projects:
  - `bench/ffi_host_boundary` (Rust staticlib + Rust baseline bench)
  - `bench/moon_ffi_boundary` (MoonBit native bench calling Rust host exports)

Benchmark scenarios:

- `rust_help`
- `moon_run_noop`
- `cached_noop`
- `daemon_cached_noop`
- `cached_binary_direct`

Run:

```bash
cd ~/code/flow
f bench-ai-runtime --iterations 80 --warmup 10 --json-out /tmp/flow_ai_runtime_bench.json
```

This is the baseline gate to ensure refactors do not regress p95 latency.

Run boundary-only microbench:

```bash
f bench-ffi-boundary --iters 10000000 --json-out /tmp/flow_ffi_boundary.json
```

## 3. Zero-Cost Boundary Design (MoonBit <> Rust)

### 3.1 Recommended boundary model

Use a narrow C ABI with primitive handles, not JSON strings, for hot paths.

- Rust hosts the scheduler, caches, daemon, security, lifecycle.
- MoonBit tasks compile to native and call host exports via `extern "C"`.
- Data boundary uses:
  - integers/enums for operation IDs and status
  - offsets/lengths into shared byte buffers for string/bytes payloads
  - opaque handles for host-managed resources

Why: this minimizes allocation/serialization churn and gives a predictable ABI.

### 3.2 ABI contract for hot calls

Candidate host functions:

- `flow_host_now_ns() -> u64`
- `flow_host_log(level: u32, ptr: *const u8, len: u32) -> i32`
- `flow_host_spawn(cmd_ptr, cmd_len, argv_ptr, argv_len, out_handle) -> i32`
- `flow_host_read_file(path_ptr, path_len, out_handle) -> i32`
- `flow_host_git(op: u32, in_handle, out_handle) -> i32`
- `flow_host_drop_handle(handle: u32)`

For MoonBit string interop helpers, the `justjavac/ffi` package is useful for C-string/wide-string conversions, but should remain at the edge of the ABI where text crossing is required.

### 3.3 Boundary rules for latency

- No JSON over FFI in hot loops.
- No per-call dynamic symbol resolution.
- Keep calls idempotent and batch-friendly.
- Use borrow/owned annotations carefully on MoonBit side to avoid refcount overhead bugs.
- Prefer fixed buffers + explicit lengths over repeated string allocations.

## 4. Refactor Roadmap

### Phase A (now)

- Keep current cached runtime + daemon.
- Add benchmark gates and require p95 non-regression before merge.

### Phase B

- Extract unified `AiTaskExecutor` in Rust.
- Route all task entrypoints through one policy engine + one telemetry schema.

### Phase C

- Add `ai_task_host` C ABI layer in Rust.
- Migrate one hot operation from shell to typed host call as a benchmarked pilot.

### Phase D

- Expand typed host API surface for common task operations.
- Keep shell fallback for compatibility.

## 5. Regression Gates

Use these checks before approving runtime changes:

1. `f bench-ai-runtime --iterations 80 --warmup 10 --json-out ...`
2. Compare p95 for:
   - `cached_noop`
   - `daemon_cached_noop`
3. Require:
   - no worse than +10% p95 vs baseline on same machine/load
   - no task failures

## 6. What "good" looks like

- Rust rebuilds become rare for workflow-level changes.
- Most iteration happens in `.ai/tasks/*.mbt`.
- Hot-path operations cross Rust/MoonBit boundary with primitive ABI payloads and stable p95 latency.
