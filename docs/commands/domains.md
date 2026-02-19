# `f domains`

Manage shared local `*.localhost` routing with a single proxy on port `80`.

## Why

Without this, each repo can start its own nginx/docker proxy and race for port `80`.
`f domains` centralizes ownership into one container: `flow-local-domains-proxy`.

State lives in:

- `~/.config/flow/local-domains/routes.json`
- generated nginx/docker files under `~/.config/flow/local-domains/`

## Commands

```bash
f domains list
f domains add linsa.localhost 127.0.0.1:3481
f domains rm linsa.localhost
f domains up
f domains down
f domains doctor
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

## Recommended Repo Pattern

Instead of per-repo docker proxy tasks:

```toml
[[tasks]]
name = "domains-up"
command = "f domains add myapp.localhost 127.0.0.1:3000 && f domains up"
```

This keeps one proxy process for all repos and avoids accidental domain hijacking.
