# `f seq-rpc`

Native `seqd` RPC bridge for Flow.

Use this when an agent/workflow needs OS-level actions and you want a typed, low-overhead path.
This command talks to `seqd` over Unix socket directly from Rust (no `seq rpc` subprocess).

## Why this command exists

- Keeps protocol handling in Rust.
- Avoids shell output parsing drift.
- Gives stable response envelope fields (`ok`, `op`, `dur_us`, ids).
- Matches hard policy in `docs/seq-agent-rpc-contract.md`.

## Usage

```bash
f seq-rpc [--socket PATH] [--timeout-ms 5000] [--pretty] <action> ...
```

Actions:

- `ping`
- `app-state`
- `perf`
- `open-app <name>`
- `open-app-toggle <name>`
- `screenshot <path>`
- `rpc <op> [--args-json '{...}']`

Common id fields (recommended on every call):

- `--request-id`
- `--run-id`
- `--tool-call-id`

Example:

```bash
f seq-rpc open-app "Safari" \
  --request-id req-42 \
  --run-id run-a12 \
  --tool-call-id tool-7 \
  --pretty
```

## Socket resolution

1. `--socket <path>`
2. `SEQ_SOCKET_PATH`
3. `SEQD_SOCKET`
4. `/tmp/seqd.sock`

## Output

Prints JSON response envelope from `seqd`.

On `ok=false`, command exits non-zero after printing the response JSON.
