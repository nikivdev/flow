# Process Tracking System for Flow CLI

## Overview

Replace the unreliable `sysinfo`-based cwd scanning with PID-based process tracking. Track PIDs when flow starts tasks, store in a global state file, and provide `f ps` and `f kill` commands.

## Design Decisions

1. **Storage**: `~/.config/flow/running.json` (global file with project paths as keys)
2. **Child Processes**: Use Unix process groups (PGID) to track and kill entire process trees
3. **Cleanup**: Validate PIDs on read, remove stale entries automatically
4. **Kill Signal**: SIGTERM first, SIGKILL after 5s timeout (configurable)

## Data Structure

```json
{
  "projects": {
    "/path/to/flow.toml": [
      {
        "pid": 12345,
        "pgid": 12345,
        "task_name": "dev",
        "command": "pnpm run dev",
        "started_at": 1701388800000,
        "config_path": "/path/to/flow.toml",
        "project_root": "/path/to/project",
        "used_flox": false
      }
    ]
  }
}
```

## Implementation

### New Module: `src/running.rs`

PID tracking state management:
- `RunningProcess` struct with pid, pgid, task_name, command, timestamps, paths
- `RunningProcesses` struct mapping config paths to process lists
- `load_running_processes()` - load and validate (remove dead PIDs)
- `save_running_processes()` - atomic write (temp file then rename)
- `register_process()` / `unregister_process()` - add/remove entries
- `get_project_processes()` - get processes for specific project
- `process_alive()` - check if PID exists
- `get_pgid()` - get process group ID for a PID

### Modified: `src/tasks.rs`

In `run_command_with_tee()`:

1. Create new process group on spawn:
```rust
#[cfg(unix)]
{
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}
```

2. After spawn, register process with `running::register_process()`
3. After wait completes, unregister with `running::unregister_process()`
4. Pass task context (name, command, paths) through to enable registration

### Modified: `src/cli.rs`

Enhanced `ProcessOpts`:
```rust
pub struct ProcessOpts {
    #[arg(long, default_value = "flow.toml")]
    pub config: PathBuf,
    #[arg(long)]
    pub all: bool,  // Show all projects
}
```

New `KillOpts`:
```rust
pub struct KillOpts {
    #[arg(long, default_value = "flow.toml")]
    pub config: PathBuf,
    pub task: Option<String>,  // Kill by task name
    #[arg(long)]
    pub pid: Option<u32>,      // Kill by PID
    #[arg(long)]
    pub all: bool,             // Kill all for project
    #[arg(long, short)]
    pub force: bool,           // SIGKILL immediately
    #[arg(long, default_value_t = 5)]
    pub timeout: u64,          // Seconds before SIGKILL
}
```

New `Kill(KillOpts)` command in Commands enum.

### Rewritten: `src/processes.rs`

Replace sysinfo-based scanning with PID-based lookup:
- `show_project_processes()` - list from running.json
- `show_all_processes()` - list all projects
- `kill_processes()` - dispatch to kill_by_pid/task/all
- `terminate_process_group()` - SIGTERM then SIGKILL with timeout

### Modified: `src/main.rs` and `src/lib.rs`

- Added `running` module export
- Added `Commands::Kill` handler

## File Changes Summary

| File | Action |
|------|--------|
| `src/running.rs` | NEW - PID tracking state |
| `src/tasks.rs` | MODIFY - process groups, register PIDs |
| `src/processes.rs` | REWRITE - PID-based lookup |
| `src/cli.rs` | MODIFY - Kill command, enhance ProcessOpts |
| `src/main.rs` | MODIFY - Kill handler |
| `src/lib.rs` | MODIFY - export running module |
| `Cargo.toml` | MODIFY - remove sysinfo dependency |

## Usage

```bash
f ps                    # List processes for current project
f ps --all              # List all flow processes
f kill dev              # Kill task by name
f kill --pid 12345      # Kill by PID
f kill --all            # Kill all for project
f kill --force dev      # SIGKILL immediately
```

## Edge Cases

- **Process dies before unregister**: Cleaned up on next `load_running_processes()`
- **Multiple tasks with same name**: All killed by `f kill <name>`
- **Flow crashes**: Orphaned processes shown in `f ps`, killed on next run
- **Race conditions**: Atomic file writes prevent corruption
