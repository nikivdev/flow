# Local Domains, No Random Ports

This pattern gives you stable local URLs like `http://maple.localhost` instead of remembering `localhost:3471`, `localhost:3472`, etc.

If you are on recent Flow, prefer shared ownership via `f domains` (see `docs/commands/domains.md`) so only one proxy binds port `80` across all repos.

It is fast and lightweight:
- One local reverse proxy process (nginx).
- No system-wide DNS daemon required.
- No VPN or packet filter changes.

## Why `.localhost`

Use `*.localhost` hostnames. They resolve to loopback by design, so traffic stays on your machine.

That means:
- `maple.localhost` can map to your web app.
- `api.maple.localhost` can map to your API.
- `ingest.maple.localhost` can map to your ingest endpoint.

## Compose Pattern

Add a tiny proxy service and route by `Host` header.

`docker-compose.yml`:

```yaml
services:
  web:
    ports:
      - "3471:80"

  api:
    ports:
      - "3472:3472"

  ingest:
    ports:
      - "3474:3474"

  local-domain-proxy:
    image: nginx:1.27-alpine
    profiles: ["domains"]
    ports:
      - "80:80"
    volumes:
      - ./docker/local-domain-proxy/nginx.conf:/etc/nginx/conf.d/default.conf:ro
    depends_on:
      - web
      - api
      - ingest
```

`docker/local-domain-proxy/nginx.conf`:

```nginx
server {
  listen 80;
  server_name maple.localhost;
  location / {
    proxy_pass http://web:80;
  }
}

server {
  listen 80;
  server_name api.maple.localhost;
  location / {
    proxy_pass http://api:3472;
  }
}

server {
  listen 80;
  server_name ingest.maple.localhost;
  location / {
    proxy_pass http://ingest:3474;
  }
}
```

Bring it up:

```bash
docker compose --profile domains up -d --build
```

## Flow Integration (`flow.toml`)

You can make this one-command via Flow tasks:

```toml
[[tasks]]
name = "domains-up"
command = "docker compose --profile domains up -d --build"
description = "Start local domain proxy + app stack"

[[tasks]]
name = "domains-down"
command = "docker compose --profile domains down"
description = "Stop local domain proxy + app stack"

[[tasks]]
name = "domains-status"
command = "docker compose --profile domains ps"
description = "Show local domain stack health"
```

Then run:

```bash
f domains-up
```

## Safety Notes (macOS)

This approach does not rewrite your network stack.

- It only binds local port `80` (inside Docker runtime).
- If port `80` is busy, compose will fail fast.
- Stopping the stack restores previous state.

Check who owns `80`:

```bash
lsof -nP -iTCP:80 -sTCP:LISTEN
```

## Troubleshooting

- `ERR_CONNECTION_REFUSED` on `*.localhost`: proxy is not running or port `80` failed to bind. Run `docker compose --profile domains ps`.
- Proxy container is `Created` but not `Up`: a dependency is unhealthy. Check `docker compose logs`.
- API healthcheck never passes: ensure healthcheck command exists in image (for example, do not depend on `curl` if not installed).

## Result

You keep internal service ports explicit in config, but humans use stable names:
- `http://maple.localhost`
- `http://api.maple.localhost`
- `http://ingest.maple.localhost`
