## Direnv-powered project daemons + task reruns

### Goals
- Auto-bootstrap a lightweight Flow watcher whenever you `cd` into a project that declares `flow.toml`.
- Keep the watcher persistent (per repo) instead of relying on `flow run ... --watch`.
- Let individual tasks declare `rerun_on` globs so Flow re-executes them when matching files change.
- Keep CPU/RAM overhead minimal by piggybacking on the existing watcher infrastructure (`notify` + debounced pipelines).

### Direnv integration
1. Ship a helper command: `flow project start --detach` (alias `flow project ensure`). It:
   - Detects the repo root (looks for nearest `flow.toml`), hashes the path, and stores PID/socket metadata under `~/.flow/projects/<hash>/`.
   - Starts a Tokio-based worker (`projectd`) if one isn’t running, then exits immediately.
2. Provide a one-liner for `.envrc`:
   ```sh
   if command -v flow >/dev/null 2>&1; then
       flow project start --detach >/dev/null 2>&1
   fi
   ```
   Direnv evaluates `.envrc` automatically when you `cd` into the repo, so the project worker is guaranteed to exist without blocking the shell.
3. Add `flow project stop` to terminate the per-project daemon (useful before upgrades) and `flow project status` for debugging.

### Project daemon responsibilities
- Load `<project>/flow.toml` and watch it for changes (reuse config reload logic from the main hub).
- Scan `[[tasks]]` and build an in-memory table of watchers for those that opt into reruns.
- Expose a Unix socket/pipe so `flow run <task>` can ask the daemon to execute a task immediately without starting a new process (future optimization).
- Log task executions into `~/.flow/projects/<hash>/logs/<task>.log` for easy inspection.

### Task schema extension
```toml
[[tasks]]
name = "setup"
command = "cargo check"
description = "Ensure workspace compiles"
rerun_on = ["package.json", "**/*.ts"]
rerun_debounce_ms = 300          # optional override
```

- `rerun_on` is an array of glob patterns relative to the project root. By default Flow groups patterns per task, but internally it merges them so overlapping tasks share a single `notify` watcher.
- Optional `rerun_paths` can pin directories when globs are expensive (e.g., `["src", "package.json"]`).
- If `rerun_on` is omitted, the task behaves exactly as today (manual `flow run task` only).

### Execution model
1. Project daemon builds a `HashMap<PathBuf, WatchGroup>` where each group coalesces tasks watching the same directory tree.
2. For every incoming `notify` event, the daemon checks the event path against each task’s glob set, respecting per-task debounce windows.
3. Matching tasks are queued onto a small Executor (e.g., Tokio runtime limited to `N` concurrent tasks). If an invocation is already running, Flow cancels or serializes depending on `task.rerun_mode` (`"restart"` vs `"queue"`).
4. Command execution reuses the existing `execute_task` helper so environment, working dir, dependency checks, etc., remain identical.

### Performance considerations
- File watching: use a single `notify` watcher per project root, filter via globset instead of spawning watchers per task.
- Debounce: share the `notify_debouncer_mini` we already ship; default 200 ms, configurable per task.
- Idle cost: project daemon stays dormant (only watchers + async runtime). Provide `flow project stop` and auto-shutdown after X minutes of no events if desired.
- Logging: stream stdout/stderr to rotating log files rather than keeping everything in memory.

### Testing plan (future)
1. Unit-test glob matching + debounce behavior for `rerun_on`.
2. Integration test that `flow project start` launches a daemon, registers watchers, and re-executes a sample task when touching `package.json`.
3. Manual E2E via Direnv in a throwaway repo to ensure `.envrc` auto-start works and multiple projects can coexist.
