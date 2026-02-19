# Anonymous Telemetry Contract (Flow CLI)

Flow emits anonymous command telemetry for product/training quality when analytics is enabled.

## Event

- `type`: `flow.command.v1`
- `schema_version`: `1`
- `event_id`: UUID
- `name`: normalized command path (for unknown commands: `task-shortcut`)
- `ok`: command success
- `at`: event timestamp (ms)
- `source`: `flow-cli`
- `payload`:
  - `anon_user_id` (rotates every 30 days)
  - `project_fingerprint` (rotates every 30 days)
  - `command_path`
  - `success`
  - `exit_code` (currently null)
  - `duration_ms`
  - `flags_used`
  - `flow_version`
  - `os`
  - `arch`
  - `interactive`
  - `ci`

## Privacy Guarantees

- No usernames/emails.
- No prompts/assistant messages.
- No file contents.
- No absolute paths (project fingerprint is HMAC-based and rotated).
- No stable raw install identifier in payload.

## Endpoint

Default endpoint:

`https://api.myflow.sh/api/telemetry/flow`

Can be overridden in `flow.toml`:

```toml
[analytics]
endpoint = "https://api.myflow.sh/api/telemetry/flow"
enabled = true
sample_rate = 1.0
```
