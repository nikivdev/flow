# f failure

Inspect and copy recent task failures without manually selecting terminal text.

## Usage

```bash
f failure
f failure last
f failure last --json
f failure list
f failure list --json --limit 10
f failure copy
f failure copy --format codex
f failure copy --format codex --write-repo
f failure copy --format claude
f failure copy --format excerpt
f failure copy --format json
```

## What It Reads

Flow records failures to a stable latest pointer plus versioned history:

```text
~/.cache/flow/last-task-failure.json
~/.cache/flow/task-failures/*.json
```

Override the latest pointer with:

```bash
FLOW_FAILURE_BUNDLE_PATH=/tmp/last-task-failure.json
```

## Notes

- `f failure` defaults to `f failure last`
- `f failure copy` defaults to the latest failure in `prompt` format
- `f failure copy --format codex` and `--format claude` render Flow-native repair prompts
- `f failure copy --write-repo` writes the selected payload to `.ai/internal/failures/`
- `f failure list --json` is intended for launcher consumers such as Alfred or Lin
- Set `FLOW_NO_CLIPBOARD=1` to disable clipboard writes
