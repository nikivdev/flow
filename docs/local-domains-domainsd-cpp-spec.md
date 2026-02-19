# Local Domains Native Daemon (C++) Spec

This document defines the native local-domains path for Flow.

Goal:
- keep stable `*.localhost` names,
- remove docker/nginx runtime dependency,
- provide low-overhead local routing suitable for agent-heavy workflows.

## Scope

This is an incremental migration path.

Phase 1 (implemented now):
- opt-in engine: `f domains --engine native ...`
- experimental C++ daemon (`domainsd-cpp`) built by Flow with `clang++`
- host-based HTTP/1.1 routing from `~/.config/flow/local-domains/routes.json`
- WebSocket upgrade passthrough
- request-side chunked transfer-encoding decode
- upstream keepalive connection pooling for safe HTTP/1.1 reuse
- bounded active handler slots with overload shedding (`503`)
- upstream connect/read/write timeouts (`504` for upstream connect timeout)
- health endpoint: `/_flow/domains/health`
- macOS launchd socket-activation installer for native privileged `:80` bind without Docker

Phase 2:
- better connection pooling and backpressure controls
- optional HTTP/2/TLS frontend mode

Phase 3:
- optional HTTPS + HTTP/2
- structured trace export for agent context

## Non-goals (for current phase)

- system DNS changes
- packet filter / firewall manipulation
- replacing `.localhost` conventions

## Control plane

Flow CLI remains the control plane:

```bash
f domains list
f domains add app.localhost 127.0.0.1:3000
f domains rm app.localhost
f domains --engine native up
f domains --engine native down
f domains --engine native doctor
```

Engine selection:
- CLI flag: `--engine docker|native`
- env fallback: `FLOW_DOMAINS_ENGINE=native`
- default: `docker`

## State

Current state remains:
- routes: `~/.config/flow/local-domains/routes.json`

Native runtime artifacts:
- pid: `~/.config/flow/local-domains/domainsd.pid`
- log: `~/.config/flow/local-domains/domainsd.log`
- built daemon binary: `~/.config/flow/local-domains/domainsd-cpp`
- macOS launchd plist (optional): `/Library/LaunchDaemons/dev.flow.domainsd.plist`

Native tuning env vars (read by `f domains --engine native up` and passed to daemon):
- `FLOW_DOMAINS_NATIVE_MAX_ACTIVE_CLIENTS` (default `128`)
- `FLOW_DOMAINS_NATIVE_UPSTREAM_CONNECT_TIMEOUT_MS` (default `10000`)
- `FLOW_DOMAINS_NATIVE_UPSTREAM_IO_TIMEOUT_MS` (default `15000`)
- `FLOW_DOMAINS_NATIVE_CLIENT_IO_TIMEOUT_MS` (default `30000`)
- `FLOW_DOMAINS_NATIVE_POOL_MAX_IDLE_PER_KEY` (default `8`)
- `FLOW_DOMAINS_NATIVE_POOL_MAX_IDLE_TOTAL` (default `256`)
- `FLOW_DOMAINS_NATIVE_POOL_IDLE_TIMEOUT_MS` (default `15000`)
- `FLOW_DOMAINS_NATIVE_POOL_MAX_AGE_MS` (default `120000`)

## Native daemon protocol (current)

Listen address:
- `127.0.0.1:80`

macOS launchd mode:
- daemon can be started with `--launchd-socket domainsd` (socket inherited from launchd)
- launchd owns privileged bind; daemon runs as local user

Routing:
- Read `Host` header (strip `:port`)
- Lookup `host -> target` from `routes.json`
- Forward request to target (`host:port`)

Special endpoint:
- `GET /_flow/domains/health`
- returns header `X-Flow-Domainsd: 1`

Errors:
- `404` when host route is not configured
- `502` on upstream connection/forward failures
- `503` when proxy is saturated and sheds overload
- `504` on upstream connect timeout
- `400` for malformed HTTP requests

## Reliability guardrails

- Keep Docker engine as default during hardening.
- Native engine is explicit opt-in.
- `f domains doctor` should always show effective owner on port 80.
- Any startup failure must surface log path directly.

## Performance targets

- added local proxy latency p99 < 1ms for tiny responses
- idle CPU near zero
- route update visibility < 100ms (mtime-based reload)

## Validation checklist

```bash
f domains add myflow.localhost 127.0.0.1:3000
sudo ./tools/domainsd-cpp/install-macos-launchd.sh   # macOS when direct bind is denied
f domains --engine native up
f domains --engine native doctor
curl -H 'Host: myflow.localhost' http://127.0.0.1/
sudo ./tools/domainsd-cpp/uninstall-macos-launchd.sh # optional teardown
```

## Next implementation work

1. Add per-upstream timeouts (connect, first-byte, total) with explicit 502/504 mapping.
2. Add end-to-end trace summary output for agents (route misses, upstream failures, latency buckets).
3. Add launchd install/uninstall tasks for persistent local startup.
4. Add optional HTTPS + HTTP/2 frontend mode.
