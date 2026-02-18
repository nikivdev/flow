# MoonBit AI Task Fast Path Guide

This guide is the practical playbook for running Flow MoonBit AI tasks at the lowest possible invocation latency.

It covers:

- when to use `f` vs `fai`
- how daemon mode actually works
- exact env knobs for tuning
- benchmark workflow to validate improvements
- troubleshooting for common regressions

---

## 1. Runtime Modes

Flow supports multiple runtime paths for `.ai/tasks/*.mbt`:

1. `moon run` path  
   `FLOW_AI_TASK_RUNTIME=moon-run f ai:flow/dev-check`  
   Highest flexibility, highest overhead.

2. Cached binary path through `f`  
   `FLOW_AI_TASK_RUNTIME=cached f ai:flow/dev-check`  
   Uses build cache, still pays full `f` process startup.

3. Daemon path through `f`  
   `f tasks run-ai --daemon ai:flow/dev-check`  
   Uses `ai-taskd` over Unix socket, still pays `f` process startup.

4. Fast daemon client (`fai`)  
   `fai ai:flow/dev-check`  
   Lowest invocation overhead for hot loops.

---

## 2. Recommended Setup (Low Latency)

From `~/code/flow`:

```bash
f install-ai-fast-client
f tasks daemon start
```

What this gives you:

- `~/.local/bin/fai` installed (low-overhead client).
- `ai-taskd` running and warm (`~/.flow/run/ai-taskd.sock`).

Verify:

```bash
which fai
fai --help
f tasks daemon status
fai ai:flow/noop
```

For always-on daemon across login sessions (recommended for stable latency):

```bash
f ai-taskd-launchd-install
f ai-taskd-launchd-status
```

---

## 3. Fast-Path Architecture

### `fai` path

1. `fai` sends a compact request to `~/.flow/run/ai-taskd.sock`
2. `ai-taskd` resolves task selector (fast exact path first)
3. `ai-taskd` reuses cached binary artifact when available
4. task process runs with `FLOW_AI_TASK_PROJECT_ROOT` set

### Key optimizations in current implementation

- daemon discovery cache with TTL:
  - `FLOW_AI_TASKD_DISCOVERY_TTL_MS` (default `750`)
- daemon artifact cache with TTL:
  - `FLOW_AI_TASKD_ARTIFACT_TTL_MS` (default `1500`)
- fast selector resolution:
  - exact selectors skip full recursive task discovery
- faster cache key computation:
  - file metadata fingerprints instead of full content hashing
  - Moon version cached on disk with TTL

Moon version knobs:

- `FLOW_AI_TASK_MOON_VERSION` (explicit override)
- `FLOW_AI_TASK_MOON_VERSION_TTL_SECS` (default `43200`)

Wire protocol knobs:

- `fai --protocol msgpack` (default)
- `fai --protocol json` (compat / debugging)

---

## 4. Using `f` with Fast Client Preference

`f` can optionally route AI task dispatch through the fast client when daemon mode is enabled.

Required:

```bash
export FLOW_AI_TASK_DAEMON=1
export FLOW_AI_TASK_FAST_CLIENT=1
```

Optional selector control:

```bash
export FLOW_AI_TASK_FAST_SELECTORS='ai:flow/noop,ai:flow/bench-cli,ai:project/*'
```

Optional client binary override:

```bash
export FLOW_AI_TASK_FAST_CLIENT_BIN="$HOME/.local/bin/fai"
```

Without `FLOW_AI_TASK_FAST_CLIENT=1`, `f` keeps normal daemon behavior.

---

## 5. `fai` CLI Usage

```bash
fai [--root PATH] [--socket PATH] [--protocol json|msgpack] [--no-cache] [--capture-output] [--timings] <selector> [-- <args...>]
fai [--root PATH] [--socket PATH] [--protocol json|msgpack] [--no-cache] [--capture-output] [--timings] --batch-stdin
```

Examples:

```bash
fai ai:flow/noop
fai --root ~/code/flow ai:flow/bench-cli -- --iterations 50
fai --no-cache ai:flow/dev-check
fai --timings ai:flow/noop
printf 'ai:flow/noop\nai:flow/noop\n' | fai --batch-stdin
```

Notes:

- default is no-capture mode for lower overhead
- use `--capture-output` if you need command output returned through client response
- use `--timings` to print server-side phase timings (`resolve_us`, `run_us`, `total_us`)
- use `--batch-stdin` for pooled client bursts (single client process, multiple requests)

---

## 6. Benchmark Procedure

Run baseline runtime benchmark:

```bash
f bench-ai-runtime --iterations 80 --warmup 10 --json-out /tmp/flow_ai_runtime.json
```

This includes:

- `moon_run_noop`
- `cached_noop`
- `daemon_cached_noop`
- `cached_binary_direct`
- `daemon_client_noop` (if `ai-taskd-client` binary is present)

For focused hot-loop comparisons:

```bash
python3 - <<'PY'
import subprocess,time,statistics
root='/Users/nikiv/code/flow'
cases=[
 ('f_daemon',['./target/debug/f','tasks','run-ai','--daemon','ai:flow/noop']),
 ('fai',['fai','ai:flow/noop']),
 ('f_cached',['./target/debug/f','ai:flow/noop']),
]
for name,cmd in cases:
  xs=[]
  for i in range(60):
    t0=time.perf_counter()
    p=subprocess.run(cmd,cwd=root,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    dt=(time.perf_counter()-t0)*1000
    if p.returncode!=0: raise SystemExit((name,p.returncode))
    if i>=10: xs.append(dt)
  xs=sorted(xs)
  pct=lambda p: xs[int((len(xs)-1)*p)]
  print(name,'p50',round(pct(0.5),2),'p95',round(pct(0.95),2),'mean',round(statistics.mean(xs),2))
PY
```

---

## 7. Operational Recommendations

Use this default profile for lowest latency:

```bash
export FLOW_AI_TASK_DAEMON=1
export FLOW_AI_TASK_FAST_CLIENT=1
export FLOW_AI_TASK_FAST_SELECTORS='ai:flow/*'
f tasks daemon start
```

Then:

- latency-critical loops: `fai ai:...`
- normal dev ergonomics: `f ai:...` (auto fast-client when selectors match)

---

## 8. Troubleshooting

### `fai` says cannot connect to socket

Start daemon:

```bash
f tasks daemon start
```

Check:

```bash
f tasks daemon status
ls -l ~/.flow/run/ai-taskd.sock
```

Or install persistent daemon:

```bash
f ai-taskd-launchd-install
```

### Task not found with `fai`

Use full selector:

```bash
f tasks list | rg '^ai:'
fai ai:flow/dev-check
```

### Latency suddenly regressed

Check:

```bash
ps -Ao pcpu,pmem,comm | sort -k1 -nr | head -n 20
f tasks daemon status
```

Then rerun benchmark with warmup.

### Need strict correctness instead of lowest overhead

Use `--capture-output` on `fai` for output-capture parity.

### Need in-daemon stage attribution for profiling

```bash
fai --timings ai:flow/noop
FLOW_AI_TASKD_TIMINGS_LOG=1 f tasks daemon serve
```

---

## 9. Implemented Optimization Set

Implemented in this iteration:

1. always-on daemon support via launchd tasks (`ai-taskd-launchd-*`)
2. binary request framing support (`msgpack`) in `fai` + `ai-taskd`
3. pooled client burst mode via `fai --batch-stdin`
4. per-request stage timings exposed via `fai --timings` and daemon timing logs

Potential next frontier:

1. keep a persistent client-side socket session with framed multi-request protocol
2. add lock-free shared-memory ring for local burst dispatch if socket overhead becomes dominant
3. push per-stage timing aggregation into benchmark JSON outputs for automatic regression gating
