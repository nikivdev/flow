# How to Use Flow to Store and Work With Env Vars

Flow stores project env vars either in cloud or locally on disk. You can
inject them into commands without ever printing values to stdout.

## Storage Backends

Flow supports two backends:

| Backend | Storage Location | Use Case |
|---------|-----------------|----------|
| `cloud` | Cloud (myflow.sh) | Team sharing, Cloudflare deploys |
| `local` | `~/.config/flow/env-local/` | Solo dev, offline, no account needed |

### Choosing a Backend

Set in `~/.config/flow/config.ts`:

```ts
export default {
  flow: {
    env: {
      backend: "local"  // or "cloud"
    }
  }
}
```

Or set the environment variable:

```bash
export FLOW_ENV_BACKEND=local
```

If neither is set, Flow tries cloud first and prompts to fall back to local
storage when unavailable.

---

## Local Storage (No Account Needed)

The simplest way to use env vars without any cloud service.

### Quick Start

```bash
# Force local backend
export FLOW_ENV_BACKEND=local

# Store a secret
f env set API_KEY=sk-xxx

# Store a personal/global secret
f env set --personal ANTHROPIC_API_KEY=sk-xxx

# Run with secrets injected
f env run -- npm start
f env run --personal -k ANTHROPIC_API_KEY -- ./my-script
```

### How Local Storage Works

Env vars are stored as plain `.env` files:

```
~/.config/flow/env-local/
├── <project-name>/
│   ├── production.env
│   ├── staging.env
│   └── dev.env
└── personal/
    └── production.env
```

Each file is a standard `.env` format:

```bash
# Environment: production
API_KEY=sk-xxx
DATABASE_URL=postgres://...
```

### Security Note

Local env files are **not encrypted**. They rely on filesystem permissions.
For sensitive production secrets, consider using cloud with Touch ID gating.

---

## Cloud Storage

For team sharing and Cloudflare deployments.

### Prerequisites

1) Create a cloud API token.
2) Login once:

```bash
f env login
```

### Bootstrapping cloud (first deploy)

If the cloud backend is not deployed yet, use local storage to deploy it once:

```bash
export FLOW_ENV_BACKEND=local
f env set --personal CLOUDFLARE_API_TOKEN=...
f deploy web
```

After `https://myflow.sh` is live, create a cloud token there and run:

```bash
f env login
```

## Store envs in cloud

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

Use the wizard to pick a .env file, select keys, and push to cloud:

```bash
f env setup
```

If your project has `[cloudflare].env_source = "cloud"`, `f env setup` uses
the keys from `env_keys` and `env_vars` and prompts only for missing values.

## Guided setup from flow.toml

If your project declares required keys in `flow.toml`, you can prompt for
missing values:

```bash
f env guide
f env guide -e staging
```

This uses `[cloudflare].env_keys` and `[cloudflare].env_vars` to decide what
to ask for.

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

To store project envs under a personal space, set in `flow.toml`:

```toml
env_space = "nikiv"
env_space_kind = "personal"
```

## Apply envs to Cloudflare Workers

Set the Cloudflare section in `flow.toml`:

```toml
[cloudflare]
path = "packages/web"
env_source = "cloud"
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
- The cloud environment defaults to `production` or uses
  `cloudflare.environment` if set.

## Example: Provision Jazz worker envs

Create a Jazz worker account and store env vars in cloud:

```bash
f db jazz new --name gitedit-mirror \
  --peer "wss://cloud.jazz.tools/?key=jazz-gitedit-prod" \
  --environment production
```

This writes `JAZZ_MIRROR_ACCOUNT_ID` and `JAZZ_MIRROR_ACCOUNT_SECRET` to cloud,
which you can then apply to Cloudflare with `f env apply`.

## Notes

- Secrets are never printed to stdout.
- `f env apply` uses project envs only (not personal).
- Keep `APP_BASE_URL` in vars (not secrets) so it shows up in Wrangler vars.
