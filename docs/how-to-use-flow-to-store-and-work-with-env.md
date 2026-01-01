# How to Use Flow to Store and Work With Env Vars

Flow stores project env vars in 1focus and can apply them to runtimes like
Cloudflare Workers. You can also fetch and inject them locally without ever
printing the values.

## Prerequisites

1) Create a 1focus API token.
2) Login once:

```bash
f env login
```

## Store envs in 1focus

Push a local .env file:

```bash
f env push
f env push -e staging
```

Set or delete individual keys:

```bash
f env set KEY=value
f env delete KEY1 KEY2
```

List keys (masked values):

```bash
f env list
```

## Interactive setup (TUI)

Use the wizard to pick a .env file, select keys, and push to 1focus:

```bash
f env setup
```

## Pull envs back to a local .env

```bash
f env pull
f env pull -e production
```

## Use envs without printing them

Fetch values:

```bash
f env get OPENAI_API_KEY -f value
f env get KEY1 KEY2 -f json
```

Run a command with injected envs:

```bash
f env run -- node server.js
f env run -k API_KEY -k SECRET -- ./app
```

Use personal envs instead of project envs:

```bash
f env get --personal KEY
f env run --personal -- npm start
```

## Apply envs to Cloudflare Workers

Set the Cloudflare section in `flow.toml`:

```toml
[cloudflare]
path = "packages/web"
env_source = "1focus"
env_keys = [
  "DATABASE_URL",
  "BETTER_AUTH_SECRET",
  "APP_BASE_URL",
  "OPENROUTER_API_KEY",
  "OPENROUTER_MODEL",
]
env_vars = ["APP_BASE_URL", "OPENROUTER_MODEL"]
environment = "production"
```

Then apply envs to the worker:

```bash
f env apply
```

- Keys in `env_vars` are set as `wrangler vars set`.
- Everything else is set as `wrangler secret put`.
- The 1focus environment defaults to `production` or uses
  `cloudflare.environment` if set.

## Example: Provision Jazz worker envs

Create a Jazz worker account and store env vars in 1focus:

```bash
f storage jazz new --name gitedit-mirror \
  --peer "wss://cloud.jazz.tools/?key=jazz-gitedit-prod" \
  --environment production
```

This writes `JAZZ_MIRROR_ACCOUNT_ID` and `JAZZ_MIRROR_ACCOUNT_SECRET` to 1focus,
which you can then apply to Cloudflare with `f env apply`.

## Notes

- Secrets are never printed to stdout.
- `f env apply` uses project envs only (not personal).
- Keep `APP_BASE_URL` in vars (not secrets) so it shows up in Wrangler vars.
