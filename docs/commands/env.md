# f env

Sync project environment and manage environment variables.

## Overview

Manage environment variables via cloud or local storage. Supports:
- Project-level environment variables
- Personal/global variables
- Multiple environments (dev, staging, production)
- Direct injection into commands
- Touch ID gating for cloud reads and keychain-backed personal local reads on macOS
- Client-side sealed project env sharing in the cloud

## Storage Backends

| Backend | Location | Config |
|---------|----------|--------|
| `cloud` | Cloud (myflow.sh) | Default, requires login |
| `local` | `~/.config/flow/env-local/` | No account needed |

Cloud behavior:
- Personal cloud envs use Flow's existing server-managed secret storage.
- Project cloud envs are sealed client-side before upload and decrypted locally on read.
- If a host deploy is configured with `env_source = "cloud"` plus a `service_token`, Flow keeps a compatibility plaintext mirror for those project keys until the host fetch path is upgraded.

Force local backend:

```bash
# Via environment variable
export FLOW_ENV_BACKEND=local

# Or in ~/.config/flow/config.ts
export default {
  flow: { env: { backend: "local" } }
}
```

If the current project has an unambiguous deploy env source such as:

```toml
[host]
env_source = "local"
```

then `f env` will also use the local backend automatically in that project.

### Local Storage Structure

```
~/.config/flow/env-local/
├── <project-name>/
│   ├── production.env
│   ├── staging.env
│   └── dev.env
└── personal/
    └── production.env
```

Storage behavior:
- Project-local envs are private `.env` files under `~/.config/flow/env-local/`.
- On macOS, personal-local env values are stored in Keychain by default; `personal/production.env` keeps Flow-managed references, not raw secret values.
- If `FLOW_ENV_LOCAL_PLAINTEXT=1` is set, Flow falls back to plaintext personal local storage.
- Local env paths are written with owner-only permissions.

## Quick Start

```bash
# Store a personal secret
f env set API_KEY=sk-xxx

# List variables (default action when logged in)
f env

# Run command with env vars injected
f env run -- npm start

# Get a value
f env get API_KEY -f value
```

## Linsa/TestFlight Example

For project-scoped keys used by local + ship flows (for example, assistant keys):

```bash
# Store in project space (recommended for app/server env)
f env project set -e dev OPENROUTER_API_KEY=sk-...
f env project set -e production OPENROUTER_API_KEY=sk-...
f env project set -e dev OPENROUTER_MODEL=anthropic/claude-sonnet-4.5
f env project set -e production OPENROUTER_MODEL=anthropic/claude-sonnet-4.5
```

Notes:
- Do not commit secrets to docs or repository files.
- Use `f env get -e production -f value OPENROUTER_API_KEY` at runtime when a ship script needs to inject a missing key.

## Subcommands

| Command | Description |
|---------|-------------|
| `login` | Authenticate with cloud |
| `set` | Set a single env var |
| `get` | Get specific env var(s) |
| `list` | List env vars for this project |
| `delete` | Delete env var(s) |
| `pull` | Fetch env vars and write to .env |
| `push` | Push local .env to cloud |
| `apply` | Apply env vars to Cloudflare worker |
| `setup` | Interactive wizard to push env vars |
| `run` | Run command with env vars injected |
| `status` | Show current auth status |
| `keys` | Show configured env keys from flow.toml |
| `sync` | Sync project settings and hub workflow |
| `bootstrap` | Bootstrap Cloudflare secrets from flow.toml |
| `unlock` | Unlock env reads (Touch ID on macOS) |

---

## Set

Store a personal environment variable:

```bash
# Basic set
f env set API_KEY=sk-xxx

# Personal envs always use the production personal store
f env set GITHUB_TOKEN=ghp_xxx
```

### Options

| Option | Short | Description |
|--------|-------|-------------|
| `--personal` | | Compatibility flag; `set` already targets personal envs |

## Project Set

Store a project-scoped environment variable:

```bash
f env project set -e dev DATABASE_URL=postgres://localhost/app
f env project set -e production PUBLIC_API_BASE_URL=https://api.example.com
```

Notes:
- Project cloud writes are sealed by default.
- On a new device, the first project read/write auto-registers that device as a sealer.
- If a key exists in cloud but was never shared to this device, Flow will ask you to re-save it from a device that already has access.

---

## Get

Retrieve environment variables:

```bash
# Get as KEY=VALUE
f env get API_KEY
# API_KEY=sk-xxx

# Get just the value
f env get API_KEY -f value
# sk-xxx

# Get as JSON
f env get API_KEY -f json
# {"API_KEY": "sk-xxx"}

# Get multiple
f env get API_KEY DATABASE_URL

# From personal store
f env get --personal GITHUB_TOKEN -f value
```

### Options

| Option | Short | Description |
|--------|-------|-------------|
| `--environment <ENV>` | `-e` | Environment (default: production) |
| `--format <FORMAT>` | `-f` | Output: `env`, `json`, or `value` (default: env) |
| `--personal` | | Fetch from personal store |

---

## List

List all environment variables:

```bash
# List production vars
f env list

# List staging vars
f env list -e staging
```

Output:
```
Environment: production

  API_KEY          OpenAI API key
  DATABASE_URL     PostgreSQL connection string
  REDIS_URL        -
```

---

## Run

Run a command with environment variables injected:

```bash
# Inject all project env vars
f env run -- npm start

# Inject specific keys
f env run -k API_KEY -k DATABASE_URL -- node server.js

# From personal store
f env run --personal -k GITHUB_TOKEN -- gh repo list

# Multiple keys from personal
f env run --personal -k TELEGRAM_BOT_TOKEN -k TELEGRAM_API_ID -- ./start.sh
```

### Options

| Option | Short | Description |
|--------|-------|-------------|
| `--environment <ENV>` | `-e` | Environment (default: production) |
| `--keys <KEYS>` | `-k` | Specific keys to inject (repeatable) |
| `--personal` | | Fetch from personal store |

### Examples

```bash
# Start app with production secrets
f env run -- npm run start

# Run with staging environment
f env run -e staging -- npm run dev

# Telegram bot with personal tokens
f env run --personal -k TELEGRAM_BOT_TOKEN -- pnpm tsx src/bot.ts
```

---

## Pull

Fetch all env vars and write to `.env` file:

```bash
# Write to .env
f env pull

# Pull staging vars
f env pull -e staging
```

Creates or overwrites `.env` in current directory with all project variables.

---

## Push

Upload local `.env` file to cloud:

```bash
f env push
```

Reads `.env` from current directory and stores all variables.

---

## Setup

Interactive wizard for pushing env vars:

```bash
f env setup
```

If `[cloudflare] env_source = "cloud"` is set in `flow.toml`, this runs a guided
prompt based on `env_keys`/`env_vars`. Otherwise it guides you through:
1. Reading your `.env` file
2. Selecting which keys to push
3. Confirming before upload

---

## Delete

Remove environment variables:

```bash
# Delete single key
f env delete API_KEY

# Delete multiple
f env delete API_KEY DATABASE_URL
```

---

## Login

Authenticate with cloud:

```bash
f env login
```

Prompts for API base URL and token. On macOS, the token is stored in Keychain
and Flow uses Touch ID to unlock env reads.

---

## Status

Check authentication status:

```bash
f env status
```

Output:
```
cloud Status
  Token: stored in Keychain
  API: https://myflow.sh
  Project: myproject
```

---

## Apply

Apply env vars to Cloudflare worker (uses `[cloudflare]` config in flow.toml):

```bash
f env apply
```

Requires `env_source = "cloud"` in your `[cloudflare]` config.

## Keys

Show env keys configured in `flow.toml` without printing values:

```bash
f env keys
```

## Unlock

On macOS, unlock env reads for the day (Touch ID):

```bash
f env unlock
```

---

## Environments

Flow supports three environments:

| Environment | Description |
|-------------|-------------|
| `production` | Production secrets (default) |
| `staging` | Staging/preview secrets |
| `dev` | Development secrets |

Use `-e` flag with project-scoped commands:

```bash
f env project set DATABASE_URL=postgres://staging... -e staging
f env list -e dev
f env run -e staging -- npm run preview
```

---

## Personal vs Project Variables

| Type | Flag | Scope | Use Case |
|------|------|-------|----------|
| Project | (default for `get`, `list`, `run`, `project ...`) | Current project | API keys, database URLs |
| Personal | `--personal` | Global/user | GitHub token, Telegram bot token |

Personal variables are tied to your user account, not a specific project.
`f env set` writes personal vars. Use `f env project set` for project vars.

```bash
# Use a personal token in any project
f env run --personal -k ANTHROPIC_API_KEY -- ./my-script
```

---

## Env Space Overrides

You can store project envs under a named cloud space by configuring `env_space`
and `env_space_kind` in `flow.toml`.

```toml
# flow.toml
env_space = "nikiv"
env_space_kind = "personal"
```

- `env_space_kind = "project"` (default) uses the project name.
- `env_space_kind = "personal"` routes project envs to your personal space.

This affects `f env pull`, `f env push`, `f env list`, `f env apply`, and service
token creation.

---

## Examples

### Typical Workflow

```bash
# 1. Authenticate
f env login

# 2. Set up secrets
f env project set DATABASE_URL=postgres://... -e production
f env project set API_KEY=sk-xxx -e production

# 3. Verify
f env list

# 4. Run app
f env run -- npm start
```

### Staging Deploy

```bash
# Set staging secrets
f env project set DATABASE_URL=postgres://staging... -e staging
f env project set API_KEY=sk-test-xxx -e staging

# Test with staging env
f env run -e staging -- npm run preview
```

### Personal Tokens for CLI Tools

```bash
# Store once
f env set --personal GITHUB_TOKEN=ghp_xxx
f env set --personal TELEGRAM_BOT_TOKEN=xxx

# Use anywhere
f env run --personal -k GITHUB_TOKEN -- gh repo list
```

### In flow.toml Tasks

```toml
[tasks.start]
command = "f env run -- npm start"

[tasks.bot]
command = "f env run --personal -k TELEGRAM_BOT_TOKEN -- node bot.js"
```

---

## Troubleshooting

### "Not authenticated"

Run `f env login` to authenticate with cloud.

### "No env vars found"

Check environment name - default is `production`:
```bash
f env list -e staging  # Check staging instead
```

### Variables not injecting

Ensure command comes after `--`:
```bash
f env run -k API_KEY -- npm start  # Correct
f env run -k API_KEY npm start     # May not work
```

## See Also

- [deploy](deploy.md) - Using env vars in deployments
- [docs/how-to-use-env.md](../how-to-use-env.md) - Extended usage guide
