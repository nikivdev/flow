# f env

Sync project environment and manage environment variables.

## Overview

Manage environment variables via 1focus (cloud) or local storage. Supports:
- Project-level environment variables
- Personal/global variables
- Multiple environments (dev, staging, production)
- Direct injection into commands
- Touch ID gating for env reads on macOS (1focus only)

## Storage Backends

| Backend | Location | Config |
|---------|----------|--------|
| `1focus` | Cloud (1focus.ai) | Default, requires login |
| `local` | `~/.config/flow/env-local/` | No account needed |

Force local backend:

```bash
# Via environment variable
export FLOW_ENV_BACKEND=local

# Or in ~/.config/flow/config.ts
export default {
  flow: { env: { backend: "local" } }
}
```

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

Files are plain `.env` format (not encrypted).

## Quick Start

```bash
# Store a secret
f env set API_KEY=sk-xxx -d "OpenAI API key"

# List variables (default action when logged in)
f env

# Run command with env vars injected
f env run -- npm start

# Get a value
f env get API_KEY -f value
```

## Subcommands

| Command | Description |
|---------|-------------|
| `login` | Authenticate with 1focus |
| `set` | Set a single env var |
| `get` | Get specific env var(s) |
| `list` | List env vars for this project |
| `delete` | Delete env var(s) |
| `pull` | Fetch env vars and write to .env |
| `push` | Push local .env to 1focus |
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

Store an environment variable:

```bash
# Basic set
f env set API_KEY=sk-xxx

# With description
f env set API_KEY=sk-xxx -d "OpenAI API key"

# Specific environment
f env set DATABASE_URL=postgres://... -e staging
```

### Options

| Option | Short | Description |
|--------|-------|-------------|
| `--environment <ENV>` | `-e` | Environment: dev, staging, production (default: production) |
| `--description <DESC>` | `-d` | Optional description for this env var |
| `--personal` | | Fetch from personal store (get/run only) |

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

Upload local `.env` file to 1focus:

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

If `[cloudflare] env_source = "1focus"` is set in `flow.toml`, this runs a guided
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

Authenticate with 1focus:

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
1focus Status
  Token: stored in Keychain
  API: https://1focus.ai
  Project: myproject
```

---

## Apply

Apply env vars to Cloudflare worker (uses `[cloudflare]` config in flow.toml):

```bash
f env apply
```

Requires `env_source = "1focus"` in your `[cloudflare]` config.

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

Use `-e` flag to specify:

```bash
f env set DATABASE_URL=postgres://staging... -e staging
f env list -e dev
f env run -e staging -- npm run preview
```

---

## Personal vs Project Variables

| Type | Flag | Scope | Use Case |
|------|------|-------|----------|
| Project | (default) | Current project | API keys, database URLs |
| Personal | `--personal` | Global/user | GitHub token, Telegram bot token |

Personal variables are tied to your user account, not a specific project.
`--personal` is supported for `get` and `run` (reading), while `set` writes
project env vars.

```bash
# Use a personal token in any project
f env run --personal -k ANTHROPIC_API_KEY -- ./my-script
```

---

## Env Space Overrides

You can store project envs under a named 1focus space by configuring `env_space`
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
f env set DATABASE_URL=postgres://... -d "Production database"
f env set API_KEY=sk-xxx -d "OpenAI key"

# 3. Verify
f env list

# 4. Run app
f env run -- npm start
```

### Staging Deploy

```bash
# Set staging secrets
f env set DATABASE_URL=postgres://staging... -e staging
f env set API_KEY=sk-test-xxx -e staging

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

Run `f env login` to authenticate with 1focus.

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
