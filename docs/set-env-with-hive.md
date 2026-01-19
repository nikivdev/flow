# Set Env Vars with Hive

This doc shows how to use `hive` to store env vars in Flow’s **local personal** env store.
These values are global (not tied to a repo) and are later pulled during deploy.

## Prereqs

- `hive` installed (`f deploy` in `~/code/lang/mbt/hive`)
- `env-help` installed (`f deploy-help` in `~/code/lang/mbt`)
- Flow local env backend (default on this machine)

## Recommended: editor-based paste (multi-line)

This opens your editor (Zed if installed, else nano), then saves and closes.

```bash
hive --paste env
```

Paste lines like:

```bash
STREAM_SERVER_HETZNER_HOST=u533855.your-storagebox.de
STREAM_SERVER_HETZNER_USER=u533855
STREAM_SERVER_HETZNER_PATH=/backups/streams
STREAM_SERVER_HETZNER_PORT=23
```

Save and close the editor to apply.

## One-liner (single line)

```bash
hive env STREAM_SERVER_HETZNER_HOST=u533855.your-storagebox.de STREAM_SERVER_HETZNER_USER=u533855 STREAM_SERVER_HETZNER_PATH=/backups/streams STREAM_SERVER_HETZNER_PORT=23
```

## Pipe (non-interactive)

```bash
cat <<'EOF' | hive --paste env
STREAM_SERVER_HETZNER_HOST=u533855.your-storagebox.de
STREAM_SERVER_HETZNER_USER=u533855
STREAM_SERVER_HETZNER_PATH=/backups/streams
STREAM_SERVER_HETZNER_PORT=23
EOF
```

## Verify

```bash
f env list
```

This lists envs in `personal` + `production` scope (values are masked).

## Deploy using Flow env store

From the repo using these envs (example: stream server):

```bash
cd ~/code/lang/cpp/stream
f deploy host
```

Flow writes `/opt/stream/.env` on the host using the local env store.

## Notes

- Env vars are stored at:
  `~/.config/flow/env-local/personal/production.env`
- Use `hive --paste env` whenever you need multi-line input.
