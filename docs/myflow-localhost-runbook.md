# myflow.localhost Runbook (Native Domains)

This is the concrete setup for running `~/code/myflow` with stable local domains:

- web UI: `http://myflow.localhost`
- optional API hostname: `http://api.myflow.localhost`

No random ports to remember in daily browser use.

## Prereqs

- Flow CLI available as `f`
- `clang++` installed (for native domains daemon build)
- myflow repo at `~/code/myflow`

## One-time route setup

```bash
f domains add myflow.localhost 127.0.0.1:3000 --replace
f domains add api.myflow.localhost 127.0.0.1:8780 --replace
```

## Start native domains engine

```bash
f domains --engine native up
f domains --engine native doctor
f domains list
```

Optional default (so you can run `f domains up` without `--engine native`):

```bash
export FLOW_DOMAINS_ENGINE=native
```

## Start myflow dev

```bash
cd ~/code/myflow
f dev
```

Then open:

- `http://myflow.localhost`

Notes:

- `f dev` runs web on `127.0.0.1:3000` and API on `127.0.0.1:8780`.
- myflow dev uses `/api` proxy to the local API port by default.
- `api.myflow.localhost` is useful for direct API checks, but the web app does not require it in the default `f dev` path.

## One-command mode (`f up` / `f down`)

In `~/code/myflow/flow.toml`, add:

```toml
[lifecycle]
up_task = "dev"

[lifecycle.domains]
host = "myflow.localhost"
target = "127.0.0.1:3000"
engine = "native"
remove_on_down = false
stop_proxy_on_down = false
```

Then use:

```bash
cd ~/code/myflow
f up
f down
```

`f down` will use task `down` if defined; otherwise it falls back to killing all running Flow-managed processes for the current project.

## Logs inside myflow

Use built-in myflow pages:

- `http://myflow.localhost/processes`
  - process state
  - start/stop actions
  - live per-process logs
- `http://myflow.localhost/logs`
  - focused log stream view

These pages query the local Flow daemon API (`http://127.0.0.1:9050`).

## Native domains runtime files

Native engine state is under:

- `~/.config/flow/local-domains/routes.json`
- `~/.config/flow/local-domains/domainsd.pid`
- `~/.config/flow/local-domains/domainsd.log`
- `~/.config/flow/local-domains/domainsd-cpp`

Quick checks:

```bash
curl -H 'Host: myflow.localhost' http://127.0.0.1/
curl http://127.0.0.1/_flow/domains/health
tail -f ~/.config/flow/local-domains/domainsd.log
```

## Common failures

1. `myflow.localhost` refuses connection

```bash
f domains --engine native doctor
lsof -nP -iTCP:80 -sTCP:LISTEN
```

Then ensure `f dev` is running in `~/code/myflow`.

2. Wrong app opens on `myflow.localhost`

```bash
f domains list
f domains add myflow.localhost 127.0.0.1:3000 --replace
```

3. Browser console shows `Invalid base URL: /api`

- update to latest `~/code/myflow` (this is handled in current auth client path resolution),
- hard-refresh browser cache,
- if running web manually (not via `f dev`), set an absolute API base, for example:

```bash
VITE_API_URL=http://api.myflow.localhost
```

## Stop

```bash
f domains --engine native down
```
