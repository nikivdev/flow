# `f domains`

Manage shared local `*.localhost` routing with a single proxy on port `80`.

## Why

Without this, each repo can start its own proxy and race for port `80`.
`f domains` centralizes ownership with one engine at a time.

State lives in:

- `~/.config/flow/local-domains/routes.json`
- runtime artifacts under `~/.config/flow/local-domains/`

## Engines

- `docker` (default): shared nginx container (`flow-local-domains-proxy`)
- `native` (experimental): local C++ daemon (`domainsd-cpp`)

Select engine per command:

```bash
f domains --engine docker up
f domains --engine native up
```

Or via env:

```bash
export FLOW_DOMAINS_ENGINE=native
```

## Commands

```bash
f domains list
f domains add linsa.localhost 127.0.0.1:3481
f domains rm linsa.localhost
f domains up
f domains down
f domains doctor
f domains --engine native up
f domains --engine native down
f domains --engine native doctor
```

## Behavior

- `f domains up`
  - starts shared proxy on `:80`
  - fails fast if another process/container owns port `80`
- `f domains add`
  - validates `host` ends with `.localhost`
  - validates target format `host:port`
  - refuses overwrite unless `--replace`
  - reloads proxy if already running
- `f domains doctor`
  - shows route count
  - shows current owner of port `80`
  - highlights conflict ownership

## Native notes (experimental)

- Requires `clang++` to build `tools/domainsd-cpp/domainsd.cpp`.
- Current scope is HTTP/1.1 host routing with WebSocket upgrade passthrough and upstream keepalive pooling.
- Native daemon has built-in overload shedding (`503`) and upstream timeout protection (`504` on connect timeout).
- HTTP/2/TLS are not implemented yet.
- See `docs/local-domains-domainsd-cpp-spec.md`.

### Native tuning envs

You can tune the native daemon at startup via environment variables:

```bash
FLOW_DOMAINS_NATIVE_MAX_ACTIVE_CLIENTS=128
FLOW_DOMAINS_NATIVE_UPSTREAM_CONNECT_TIMEOUT_MS=10000
FLOW_DOMAINS_NATIVE_UPSTREAM_IO_TIMEOUT_MS=15000
FLOW_DOMAINS_NATIVE_CLIENT_IO_TIMEOUT_MS=30000
FLOW_DOMAINS_NATIVE_POOL_MAX_IDLE_PER_KEY=8
FLOW_DOMAINS_NATIVE_POOL_MAX_IDLE_TOTAL=256
FLOW_DOMAINS_NATIVE_POOL_IDLE_TIMEOUT_MS=15000
FLOW_DOMAINS_NATIVE_POOL_MAX_AGE_MS=120000
```

## Recommended Repo Pattern

Instead of per-repo docker proxy tasks:

```toml
[[tasks]]
name = "domains-up"
command = "f domains add myapp.localhost 127.0.0.1:3000 && f domains up"
```

This keeps one proxy process for all repos and avoids accidental domain hijacking.
