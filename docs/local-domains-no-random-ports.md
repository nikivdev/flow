# Local Domains, No Random Ports

This pattern gives stable local URLs like `http://gen.localhost` and `http://linsa.localhost` instead of remembering random ports.

Use shared ownership via `f domains` (see `docs/commands/domains.md`) so only one proxy binds port `80` across all repos.

It is fast and lightweight:
- One shared local reverse proxy container (nginx).
- No system-wide DNS daemon required.
- No VPN or packet filter changes.

Experimental native path:
- `f domains --engine native up` runs a local C++ daemon instead of docker/nginx.
- Keep this opt-in for now; docker remains the default engine.

## Why `.localhost`

Use `*.localhost` hostnames. They resolve to loopback by design, so traffic stays on your machine.

That means:
- `gen.localhost` can map to `127.0.0.1:5001`.
- `linsa.localhost` can map to `127.0.0.1:3481`.
- `api.myflow.localhost` can map to `127.0.0.1:8780`.

## Recommended Pattern: Shared `f domains`

Register routes once, then run your normal dev servers on fixed ports.

```bash
f domains add gen.localhost 127.0.0.1:5001
f domains add linsa.localhost 127.0.0.1:3481
f domains add myflow.localhost 127.0.0.1:3000
f domains add api.myflow.localhost 127.0.0.1:8780

f domains up
f domains list
```

`f domains up` ensures the shared proxy is running. `f domains list` shows the active route table.

Native engine (experimental):

```bash
f domains --engine native up
f domains --engine native doctor
f domains --engine native down
```

On macOS, if native bind to `:80` is denied, install launchd socket mode once:

```bash
cd ~/code/flow
sudo ./tools/domainsd-cpp/install-macos-launchd.sh
```

You can also set:

```bash
export FLOW_DOMAINS_ENGINE=native
```

## Flow Task Pattern (`flow.toml`)

Use these task shapes in each repo:

```toml
[[tasks]]
name = "domains-up"
command = """
set -euo pipefail

f domains add <repo>.localhost 127.0.0.1:<port>
f domains up
"""

[[tasks]]
name = "domains-down"
command = "sh -lc 'f domains rm <repo>.localhost || true'"

[[tasks]]
name = "domains-status"
command = "f domains doctor && f domains list"
```

Example mappings used together safely:

```bash
gen.localhost           -> 127.0.0.1:5001
linsa.localhost         -> 127.0.0.1:3481
myflow.localhost        -> 127.0.0.1:3000
api.myflow.localhost    -> 127.0.0.1:8780
```

## Reliability Notes

- Keep app ports fixed (`5001`, `3481`, `3000`, `8780`) and route hostnames to them.
- Do not run per-repo proxy stacks in parallel with `f domains`; use one shared proxy owner.
- Check health with:

```bash
f domains doctor
```

## Troubleshooting

- `ERR_CONNECTION_REFUSED` on `*.localhost`: run `f domains up`, then `f domains doctor`.
- Wrong project opens on a hostname: route collision or stale mapping. Check `f domains list`, then `f domains rm <host>` and re-add.
- Port `80` bind failure: another process owns port `80`. Find it with:

```bash
lsof -nP -iTCP:80 -sTCP:LISTEN
```

## Logs in myflow

If you use `myflow` as your local operations UI, open:

- `http://myflow.localhost/processes` for process status and per-process log streams.
- `http://myflow.localhost/logs` for focused live logs.

Run `f lin` first so the Flow daemon is online for these pages.

For the full end-to-end setup (`f domains --engine native`, `f dev`, health checks, and troubleshooting), see:
`docs/myflow-localhost-runbook.md`.

## Legacy Pattern (Not Recommended)

Per-repo docker-compose proxies also work, but they are easier to conflict on port `80` and cause hostname drift across repos. Prefer shared `f domains` unless you have a strict repo-isolated requirement.

## Result

You keep internal service ports explicit in config, but humans use stable names:
- `http://gen.localhost`
- `http://linsa.localhost`
- `http://myflow.localhost`
