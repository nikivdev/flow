# Task Failure Hooks

Flow can run a command automatically when a task fails. This is useful for
opening a tailored prompt, collecting diagnostics, or launching a helper tool.

## Overview

- The hook runs after a task exits with a non-zero status.
- The hook runs in the task's working directory.
- The hook only runs when stdin is a TTY (no hook in non-interactive runs).
- The hook is disabled when `FLOW_DISABLE_TASK_FAILURE_HOOK` is set.

## Where The Hook Is Configured

You can set the hook in either place:

1. Environment variable (highest priority):

```bash
export FLOW_TASK_FAILURE_HOOK='rise work --errors --diff --patch --focus --focus-app lin --target codex "fix $FLOW_TASK_NAME failure"'
```

2. Global Flow config (generated file):

- Edit `~/.config/lin/config.ts` and regenerate, or
- Edit `~/.config/flow/config.ts` directly if you know it is safe to do so.

Example entry in config:

```ts
export default {
  flow: {
    taskFailureHook: "rise work --errors --diff --patch --focus --focus-app lin --target codex \"fix $FLOW_TASK_NAME failure\""
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

## Disabling The Hook

Set the following env var:

```bash
export FLOW_DISABLE_TASK_FAILURE_HOOK=1
```

## Rise / Zed Behavior

If your hook calls `rise work`, Flow automatically appends `--no-open` unless you
explicitly allow it. This prevents Zed from opening on every failure.

To allow the open behavior:

```bash
export FLOW_TASK_FAILURE_HOOK_ALLOW_OPEN=1
```

## Example Hook

```bash
export FLOW_TASK_FAILURE_HOOK='rise work --errors --diff --patch --focus --focus-app lin --target codex "fix $FLOW_TASK_NAME failure"'
```

This will write prompts to `.rise/prompts/` and focus the codex prompt without
opening Zed by default.
