# f deploy

Deploy projects to hosts and cloud platforms.

## Overview

The `deploy` command handles deployment to multiple platforms:
- **Linux hosts** via SSH (with systemd + nginx)
- **Cloudflare Workers**
- **Railway**

Auto-detects the platform from your `flow.toml` configuration.
If `[flow].deploy_task` is set, `f deploy` runs that task first.
If no deployment config exists but a `deploy` task is defined, `f deploy` runs that task.

## Quick Start

```bash
# Auto-deploy based on flow.toml config
f deploy

# Run the project's release task (flow.release_task or fallback)
f deploy release

# Deploy to specific platform
f deploy host
f deploy cloudflare
f deploy railway
```

## Subcommands

| Command | Alias | Description |
|---------|-------|-------------|
| `host` | `h` | Deploy to Linux host via SSH |
| `cloudflare` | `cf` | Deploy to Cloudflare Workers |
| `setup` | | Interactive deploy setup (Cloudflare) |
| `railway` | | Deploy to Railway |
| `release` | | Run the project's release task |
| `status` | | Show deployment status |
| `logs` | | View deployment logs |
| `restart` | | Restart the deployed service |
| `stop` | | Stop the deployed service |
| `shell` | | SSH into the host |
| `set-host` | `set` | Configure host for deployment |
| `show-host` | | Show current host configuration |
| `health` | | Check if deployment is healthy |

---

## Host Deployment (Linux via SSH)

Deploy to any Linux server with SSH access. Flow handles:
- File syncing via rsync
- Systemd service creation
- Nginx reverse proxy setup
- SSL via Let's Encrypt

### Configuration

Add to `flow.toml`:

```toml
[flow]
deploy_task = "deploy-cli-release"

[host]
dest = "/opt/myapp"           # Remote destination path
run = "./server"              # Command to run the service
port = 3000                   # Port the service listens on
service = "myapp"             # Systemd service name (optional, defaults to folder name)
setup = "./scripts/setup.sh"  # Setup script to run after first sync (optional)
env_file = ".env.production"  # Path to .env file for secrets (optional)
domain = "myapp.example.com"  # Public domain for nginx (optional)
ssl = true                    # Enable SSL via Let's Encrypt (optional)
```

Tip: `f setup deploy` can scaffold the `[host]` section and create a remote setup script.

### Setup Host

First, configure your SSH connection:

```bash
# Set host (stored globally at ~/.config/flow/deploy.json)
f deploy set-host user@host:port
f deploy set-host deploy@myserver.com:22
f deploy set-host root@192.168.1.100

# Verify connection
f deploy shell
```

### Deploy

```bash
# Deploy to host
f deploy host

# Force re-run setup script
f deploy host --setup

# Build remotely instead of syncing local artifacts
f deploy host --remote-build
```

### What Happens

1. **Sync files** - rsync uploads project (excludes `target/`, `.git/`, `node_modules/`, `.env`, `*.log`)
2. **Copy env file** - If `env_file` is specified, copies it to `{dest}/.env`
3. **Run setup** - Executes setup script on first deploy or with `--setup`
4. **Create systemd service** - Generates and enables `/etc/systemd/system/{service}.service`
5. **Configure nginx** - If `domain` is set, creates reverse proxy config
6. **Setup SSL** - If `ssl = true`, runs certbot for Let's Encrypt certificate
7. **Start service** - Runs `systemctl restart {service}`

### Manage Service

```bash
# View logs
f deploy logs
f deploy logs -f         # Follow in real-time
f deploy logs -n 500     # Show last 500 lines

# Restart/stop
f deploy restart
f deploy stop

# Check status
f deploy status

# SSH into server
f deploy shell

# Health check
f deploy health
f deploy health --url https://myapp.example.com/health
f deploy health --status 204  # Expect specific status code
```

---

## Cloudflare Workers

Deploy to Cloudflare's edge network.

### Configuration

Add to `flow.toml`:

```toml
[cloudflare]
path = "worker"                    # Path to worker directory (optional, defaults to project root)
environment = "production"         # Wrangler environment name (optional)
env_file = ".env.cloudflare"       # Path to .env file for secrets (optional)
env_source = "1focus"              # Use 1focus for secrets (optional)
env_keys = ["API_KEY", "SECRET"]   # Specific keys to fetch from 1focus (optional)
env_vars = ["PUBLIC_URL"]          # Keys to set as non-secret vars (optional)
deploy = "wrangler deploy"         # Custom deploy command (optional)
dev = "wrangler dev"               # Custom dev command (optional)
url = "https://my-worker.workers.dev"  # URL for health checks (optional)
```

### Prerequisites

- [Wrangler CLI](https://developers.cloudflare.com/workers/wrangler/) installed
- `wrangler.toml` in your worker directory
- Authenticated with `wrangler login`

### Deploy

```bash
# Deploy
f deploy cloudflare

# Deploy with secrets from env_file
f deploy cloudflare --secrets

# Run in dev mode
f deploy cloudflare --dev
```

### Interactive Setup

For first-time setup, use the interactive wizard:

```bash
f deploy setup
```

This walks you through:
1. Selecting worker directory (auto-discovers `wrangler.toml`)
2. Choosing .env file for secrets
3. Selecting Cloudflare environment (production, staging, etc.)
4. Picking which secrets to push
5. Updating `flow.toml` with your choices

### Secrets from 1focus

If using 1focus for secret management:

```toml
[cloudflare]
env_source = "1focus"
env_keys = ["ANTHROPIC_API_KEY", "DATABASE_URL"]  # Fetched as secrets
env_vars = ["PUBLIC_API_URL"]                      # Fetched as non-secret vars
environment = "production"
```

Then deploy:

```bash
f deploy cloudflare --secrets
```

If you need to fill missing values first:

```bash
f env guide
```

---

## Railway

Deploy to Railway's platform.

### Configuration

Add to `flow.toml`:

```toml
[railway]
project = "my-project"         # Railway project ID
service = "api"                # Service name (optional)
environment = "production"     # Environment name (optional)
start = "npm start"            # Start command (optional)
env_file = ".env.railway"      # Path to .env file (optional)
```

### Prerequisites

- [Railway CLI](https://docs.railway.app/develop/cli) installed (`npm install -g @railway/cli`)
- Authenticated with `railway login`

### Deploy

```bash
f deploy railway
```

What happens:
1. Links to Railway project if specified
2. Sets environment variables from `env_file`
3. Deploys with `railway up --detach`

---

## Health Checks

Check if your deployment is responding:

```bash
# Use domain from [host] or url from [cloudflare] config
f deploy health

# Custom URL
f deploy health --url https://api.example.com/health

# Expect specific status code
f deploy health --status 204
```

Returns:
- `Healthy (HTTP 200 in 0.15s)` on success
- `Unhealthy: expected HTTP 200, got 500` on wrong status
- `Unreachable: Connection refused` on network error

---

## Global Host Configuration

Host connection is stored globally at `~/.config/flow/deploy.json`:

```json
{
  "host": {
    "user": "deploy",
    "host": "myserver.com",
    "port": 22
  }
}
```

View/set:

```bash
f deploy show-host
f deploy set-host deploy@newserver.com:2222
```

---

## Examples

### Full Host Setup

```toml
# flow.toml
[host]
dest = "/opt/api"
run = "/opt/api/server"
port = 8080
service = "myapi"
setup = "cargo build --release && cp target/release/server /opt/api/"
env_file = ".env.production"
domain = "api.mycompany.com"
ssl = true
```

```bash
# First time setup
f deploy set-host root@myserver.com

# Deploy
f deploy host

# Check it's working
f deploy health
f deploy logs -f
```

### Full Cloudflare Setup

```toml
# flow.toml
[cloudflare]
path = "packages/worker"
environment = "production"
env_source = "1focus"
env_keys = ["OPENAI_API_KEY", "WEBHOOK_SECRET"]
url = "https://my-worker.mycompany.workers.dev"
```

```bash
# Store secrets in 1focus
f env set OPENAI_API_KEY=sk-... -d "OpenAI API key"
f env set WEBHOOK_SECRET=whsec_... -d "Webhook signing secret"

# Deploy with secrets
f deploy cloudflare --secrets

# Verify
f deploy health
```

### CI/CD Integration

```yaml
# GitHub Actions
deploy:
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v4
    - name: Deploy
      run: |
        f deploy set-host ${{ secrets.DEPLOY_HOST }}
        f deploy host
        f deploy health
```

---

## Troubleshooting

### "No host configured"

Run `f deploy set-host user@host:port` first.

### "No wrangler config found"

Ensure `wrangler.toml` exists in your worker directory, or run `wrangler init`.

### SSH connection fails

Test with `f deploy shell` to debug. Check:
- SSH key is in `~/.ssh/` and added to server
- Port is correct (default: 22)
- Server is reachable

### Secrets not updating

For Cloudflare, use `--secrets` flag: `f deploy cloudflare --secrets`

### Health check fails

Check URL is correct and service is running:
```bash
f deploy logs -f  # Check for errors
curl -v https://your-domain.com  # Test manually
```
