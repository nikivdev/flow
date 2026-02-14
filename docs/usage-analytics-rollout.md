# Flow Anonymous Usage Tracking (Zero Cost) - Implementation Checklist

This is the execution plan to add opt-in anonymous usage tracking to Flow with near-zero runtime overhead.

## Goals

- Default off (or unknown until prompt), explicit user opt-in.
- No sensitive data (no prompts, no command values, no paths, no repo names).
- Command runtime must not block on network.
- Ingest through `base` trace API and store in a separate ClickHouse instance.

## Data Contract (anonymous only)

Event kind: `flow.command`

Allowed fields:

- `install_id` (random UUID, local)
- `command_path` (e.g. `commit`, `skills.sync`, `setup.deploy`)
- `success` (`true/false`)
- `exit_code` (integer or null)
- `duration_ms` (integer)
- `flags_used` (flag names only; e.g. `["sync","context"]`)
- `flow_version`
- `os`, `arch`
- `interactive` (`true/false`)
- `ci` (`true/false`)
- `project_fingerprint` (optional HMAC; never raw path/remote)
- `at` timestamp

Forbidden fields:

- prompts, command strings, args values, paths, repo URL/name, output.

## Patch Order

### Phase 1: Local capture + opt-in state (Flow only)

1. Add `src/usage.rs`
   - `UsageConfigState` (enabled/disabled/unknown, install_id, secret, last_prompt_at).
   - local queue file: `~/.config/flow/usage-queue.jsonl`.
   - append-only write API: `record_command_event(...)`.
   - sanitize and normalize command path + flag names.
2. Add config support in `src/config.rs`
   - `[analytics]` config:
     - `enabled` (`true/false` optional)
     - `endpoint` (default `http://127.0.0.1:7331/v1/trace`)
     - `sample_rate` (default `1.0`)
3. Hook command lifecycle in `src/main.rs`
   - capture start timestamp before dispatch.
   - on return/error, emit one event through `usage::record_command_event`.
   - never fail command if analytics fails.
4. Add command group in `src/cli.rs` and handler in new `src/analytics.rs`
   - `f analytics status`
   - `f analytics enable`
   - `f analytics disable`
   - `f analytics export`
   - `f analytics purge`
5. Wire module exports in `src/lib.rs` and command dispatch in `src/main.rs`.

Validation:

```bash
cd /Users/nikiv/code/flow
cargo check
cargo run --bin f -- analytics status
```

### Phase 2: Opt-in UX

1. In `src/main.rs`, after first successful interactive command:
   - if state is `unknown` and non-CI, prompt once:
     - "Enable anonymous usage tracking to improve Flow? [y/N/later]"
2. Persist response to `~/.config/flow/analytics.toml`.
3. Add env overrides:
   - `FLOW_ANALYTICS_FORCE=1` (self-test)
   - `FLOW_ANALYTICS_DISABLE=1` (hard off)

Validation:

```bash
FLOW_ANALYTICS_FORCE=1 cargo run --bin f -- tasks
cargo run --bin f -- analytics status
```

### Phase 3: Async uploader (still in Flow)

1. In `src/usage.rs` add `flush_queue_async()`
   - background thread
   - small batches (50-200)
   - short HTTP timeout (<=500ms)
   - retries with backoff
2. Upload target defaults to base trace endpoint:
   - `http://127.0.0.1:7331/v1/trace`
3. Add spool safety:
   - max queue bytes (e.g. 10MB), oldest-drop policy.

Validation:

```bash
f analytics status
# run a few commands
f tasks
f skills list
# then flush
f analytics export
```

### Phase 4: Base ingest + dedicated ClickHouse

Implement using `base` doc `docs/flow-usage-tracking.md` (added in parallel).

### Phase 5: Read path and dashboards

1. Extend `seqch` in `/Users/nikiv/code/org/linsa/base/crates/seqch-cli/src/main.rs`
   - new top-level area: `flow`
   - commands:
     - `seqch flow commands --hours 24`
     - `seqch flow flags --hours 24`
     - `seqch flow failures --hours 24`
2. Add starter SQL dashboards:
   - command usage over time
   - adoption funnel (`unknown -> enabled`)
   - failures by command

## Self-Test Rollout

1. Enable only for yourself:
   - `FLOW_ANALYTICS_FORCE=1`
2. Verify no sensitive fields in payload samples (`f analytics export`).
3. Verify ingestion into separate CH instance.
4. After 3-7 days, turn prompt on for all users (still opt-in).

## Acceptance Criteria

- P50 added runtime overhead per command < 1ms local.
- Command success path unaffected by network or ingest failures.
- No sensitive strings in stored events (spot-check samples).
- Able to answer:
  - top-used commands
  - least-used commands
  - failure hotspots by command path.
