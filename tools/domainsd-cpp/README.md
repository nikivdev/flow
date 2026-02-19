# domainsd-cpp (experimental)

`domainsd-cpp` is an experimental native local-domains proxy used by:

- `f domains --engine native up`
- `f domains --engine native down`

It is designed for low overhead on localhost routing and keeps Flow route state in:

- `~/.config/flow/local-domains/routes.json`

Current scope:
- HTTP/1.1 host-based routing (`*.localhost` -> `host:port`)
- WebSocket upgrade passthrough (full duplex tunnel)
- request-side chunked transfer-encoding decode/forward
- upstream keepalive connection pooling (safe framed-response reuse)
- overload shedding with bounded active client handlers (`503` when saturated)
- upstream connect/IO timeouts (`504` for connect timeout)
- health endpoint: `GET /_flow/domains/health`
- mtime-based route reload (no daemon restart required)

Runtime tuning:
- daemon supports CLI flags (`--max-active-clients`, `--upstream-*-timeout-ms`, `--pool-*`)
- Flow passes tuning via env vars prefixed `FLOW_DOMAINS_NATIVE_*`

Current limitations:
- no HTTP/2/TLS yet

The Flow CLI builds this binary automatically with `clang++` when needed.
