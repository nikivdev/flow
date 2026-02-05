---
name: env
description: Manage environment variables and secrets with flow (always use Flow env store)
source: flow-default
---

# Environment Variables & Secrets

Flow provides a secure way to manage environment variables across projects.

## Rules (Must)

- Always use Flow env store for secrets/tokens in Flow projects.
- Do not rely on shell env vars or `.env` files for secrets unless they are injected via `f env`.
- For tasks/scripts that need secrets, fetch them with `f env get` or run via `f env run`.

## Setup

### 1. Define variables in flow.toml

Add a `[storage]` section to your project's `flow.toml`:

```toml
[storage]
provider = "myflow.sh"

[[storage.envs]]
name = "local"
description = "Local development"
variables = [
  { key = "DATABASE_URL" },
  { key = "API_KEY" },
  { key = "SECRET_TOKEN", default = "" },
]
```

### 2. Set environment variables

Use `f env set` to store values:

```bash
# Set individual env vars
f env set API_KEY=abc123
f env set DATABASE_URL="postgres://..."

# Values are stored in ~/.config/flow/env-local/personal/production.env
```

### 3. Pull env vars to local .env

```bash
# Pull all env vars for the current environment
f env pull

# Show current env vars
f env list

# Get specific var
f env get API_KEY
```

## Commands

| Command | Description |
|---------|-------------|
| `f env set KEY=value` | Store an env var |
| `f env pull` | Pull env vars to local .env file |
| `f env push` | Push local .env to cloud |
| `f env list` | List env vars for this project |
| `f env get KEY` | Get specific env var(s) |
| `f env keys` | Show configured env keys from flow.toml |
| `f env setup` | Interactive env setup |
| `f env guide` | Guided prompt to set required vars |
| `f env run <cmd>` | Run command with env vars injected |

## Environment Selection

Flow supports multiple environments:

```toml
[[storage.envs]]
name = "local"
variables = [{ key = "DATABASE_URL" }]

[[storage.envs]]
name = "staging"
variables = [{ key = "DATABASE_URL" }]

[[storage.envs]]
name = "production"
variables = [{ key = "DATABASE_URL" }]
```

## Example: Spotify CLI

```toml
[storage]
provider = "myflow.sh"

[[storage.envs]]
name = "local"
description = "Spotify API credentials"
variables = [
  { key = "SPOTIFY_CLIENT_ID" },
  { key = "SPOTIFY_CLIENT_SECRET" },
  { key = "SPOTIFY_ACCESS_TOKEN" },
  { key = "SPOTIFY_REFRESH_TOKEN", default = "" },
]
```

Then:
```bash
# Set your credentials (example values)
f env set SPOTIFY_CLIENT_ID=example_client_id
f env set SPOTIFY_CLIENT_SECRET=example_client_secret

# Run CLI with env vars injected
f env run bun run src/main.ts now

# Or pull to .env first
f env pull
source .env
bun run src/main.ts now
```

## Task pattern (required for secrets)

When writing Flow tasks, prefer:

```bash
MY_TOKEN="$(FLOW_ENV_BACKEND=local f env get --personal MY_TOKEN -f value 2>/dev/null || true)"
if [ -z "${MY_TOKEN:-}" ]; then
  echo "MY_TOKEN missing. Save it with envnew MY_TOKEN=..."
  exit 1
fi
export MY_TOKEN
```

## One-time passwords (1Password Connect)

Use Flow's OTP command to fetch TOTP codes from 1Password Connect:

```bash
f otp get <vault> <item> [--field <label>]
```

Requires:
- `OP_CONNECT_HOST`
- `OP_CONNECT_TOKEN` (env or Flow personal env store)

## Storage Locations

- **Personal env vars:** `~/.config/flow/env-local/personal/production.env`
- **Project env vars:** Stored via myflow.sh cloud backend
- **Local .env:** Created in project root via `f env pull`

## Authentication

Flow uses a token stored in `~/.config/flow/auth.toml` to authenticate. If you haven't authenticated:

```bash
f auth login
```

## Security Notes

- Personal env vars stored locally in `~/.config/flow/`
- Project env vars can be synced to cloud via `f env push`
- Never commit `.env` files to git (add to `.gitignore`)
- Use `f env run` to inject vars without creating .env files
