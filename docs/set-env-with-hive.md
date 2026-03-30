# Set Env Vars with Flow Env

This doc replaces the old Hive-based env flow.

Use Flow's env store directly for personal env vars. These values are global, not tied to one repo, and can later be read during deploy or local tooling.

## Recommended paths

### Guided setup

If the current repo has env metadata and you want a guided prompt:

```bash
f env guide
```

### Set one personal env var

```bash
f env set STREAM_SERVER_HETZNER_HOST=u533855.your-storagebox.de
```

### Set several personal env vars

Run `f env set` once per pair:

```bash
f env set STREAM_SERVER_HETZNER_HOST=u533855.your-storagebox.de
f env set STREAM_SERVER_HETZNER_USER=u533855
f env set STREAM_SERVER_HETZNER_PATH=/backups/streams
f env set STREAM_SERVER_HETZNER_PORT=23
```

## Verify

```bash
f env list
```

This lists envs in the active Flow env store with values masked.

## Deploy using Flow env store

From the repo using these envs:

```bash
cd ~/code/lang/cpp/stream
f deploy host
```

Flow writes the target `.env` using the Flow env store.

## Notes

- Personal env vars are stored by Flow, not Hive.
- For repo-aware guided setup, prefer `f env guide`.
- For direct personal writes, prefer repeated `f env set KEY=VALUE`.
