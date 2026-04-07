# Task Failure Hooks

Flow can run a command automatically when a task fails. This is useful for
opening a tailored prompt, collecting diagnostics, or launching a helper tool.

Flow also records failures for later inspection:

```bash
f failure last
f failure list
f failure copy
```

## Overview

- The hook runs after a task exits with a non-zero status.
- The hook runs in the task's working directory.
- The hook only runs when stdin is a TTY (no hook in non-interactive runs).
- The hook is disabled when `FLOW_DISABLE_TASK_FAILURE_HOOK` is set.

## Where The Hook Is Configured

You can set the hook in either place:

1. Environment variable (highest priority):

```bash
export FLOW_TASK_FAILURE_HOOK='f failure copy --format codex --write-repo >/dev/null'
```

2. Global Flow config (generated file):

- Edit `~/.config/lin/config.ts` and regenerate, or
- Edit `~/.config/flow/config.ts` directly if you know it is safe to do so.

Example entry in config:

```ts
export default {
  flow: {
    taskFailureHook: "f failure copy --format codex --write-repo >/dev/null"
  }
}
```

## Command Execution Details

- The hook is executed with `/bin/sh -c`.
- The working directory is the task's `workdir` (the repo or task `cwd`).
- The hook inherits stdin/stdout/stderr from the task runner.

## Environment Variables Provided To The Hook

Flow sets these environment variables when the hook runs:

- `FLOW_TASK_NAME` (task name)
- `FLOW_TASK_COMMAND` (command string)
- `FLOW_TASK_WORKDIR` (absolute path)
- `FLOW_TASK_STATUS` (exit code, or `-1` if unknown)
- `FLOW_FAILURE_BUNDLE_PATH` (path to the last failure bundle)
- `FLOW_TASK_OUTPUT_TAIL` (tail of task output, truncated)

## Failure Bundle Location

Flow writes a JSON failure bundle to one of these locations:

- `FISHX_FAILURE_PATH` if set
- `FLOW_FAILURE_BUNDLE_PATH` if set
- `~/.cache/flow/last-task-failure.json` (default)

The resolved path is passed to hooks via `FLOW_FAILURE_BUNDLE_PATH`.

Flow also keeps versioned history beside that latest pointer under:

```text
~/.cache/flow/task-failures/
```

## Disabling The Hook

Set the following env var:

```bash
export FLOW_DISABLE_TASK_FAILURE_HOOK=1
```

## Recommended Flow-Native Hooks

Use one of these if you want Flow to copy a ready-made repair prompt
automatically after an interactive task failure:

```bash
export FLOW_TASK_FAILURE_HOOK='f failure copy --format codex --write-repo >/dev/null'
export FLOW_TASK_FAILURE_HOOK='f failure copy --format claude --write-repo >/dev/null'
```

These commands use the failure bundle Flow just recorded and write the rendered
prompt under `.ai/internal/failures/` in the project root.

Legacy `rise work` hooks are retired. If your config still uses them, Flow will
warn and skip the hook.

## Example Hook

```bash
export FLOW_TASK_FAILURE_HOOK='f failure copy --format codex --write-repo >/dev/null'
```

This copies a Flow-generated Codex repair prompt to the clipboard and writes
`.ai/internal/failures/latest-codex.md` after each interactive task failure.
