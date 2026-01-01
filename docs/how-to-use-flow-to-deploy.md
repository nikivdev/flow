# How to Deploy with Flow

Flow provides a unified `f deploy` command to deploy your projects to Linux hosts, Cloudflare Workers, or Railway.

## Quick Start

```bash
# Set up your deployment target (one-time)
f deploy set-host root@your-server.com:22

# Add [host] config to your flow.toml (see below)

# Deploy
f deploy
```

## Deployment Targets

Flow auto-detects the deployment target from your `flow.toml`:
- `[host]` → Linux server via SSH
- `[cloudflare]` → Cloudflare Workers
- `[railway]` → Railway

## Linux Host Deployment

### Prerequisites

- SSH access to your server (key-based auth recommended)
- `rsync` installed locally
- Server should have: systemd, nginx (optional), certbot (for SSL)

### Configuration

```toml
[host]
dest = "/opt/myapp"                    # Where to deploy on server
setup = """
cargo build --release
"""
run = "/opt/myapp/target/release/server"  # Command to start service
port = 3000                            # Port your app listens on
service = "myapp"                      # Systemd service name
env_file = ".env.production"           # Local .env to copy to server
domain = "myapp.example.com"           # Public domain (optional)
ssl = true                             # Enable Let's Encrypt SSL
```

### Setup Flow

```bash
# Configure target host (stored in ~/.config/flow/deploy.json)
f deploy set-host root@your-server.com:22

# Verify connection
f deploy shell
```

### Deploy

```bash
# Full deployment
f deploy

# Or explicitly
f deploy host

# Force re-run setup script
f deploy host --setup
```

### What Happens

1. **Sync files** via rsync (excludes: target/, .git/, node_modules/)
2. **Copy .env** from `env_file` to server
3. **Run setup** script (only on first deploy or with `--setup`)
4. **Create systemd service** with your `run` command
5. **Configure nginx** reverse proxy (if `domain` specified)
6. **Set up SSL** via certbot (if `ssl = true`)
7. **Start/restart** the service

### Management Commands

```bash
f deploy status     # Check if service is running
f deploy logs       # View recent logs
f deploy logs -f    # Follow logs in real-time
f deploy restart    # Restart the service
f deploy stop       # Stop the service
f deploy shell      # SSH into the server
```

## Cloudflare Workers

### Prerequisites

- Wrangler CLI: `npm install -g wrangler`
- Authenticated: `wrangler login`

### Configuration

```toml
[cloudflare]
path = "worker"                  # Path to worker directory
env_file = ".env.cloudflare"     # Secrets to set
env_source = "1focus"            # Use 1focus as env source (optional)
env_keys = ["API_KEY"]           # Keys to fetch from 1focus (optional)
env_vars = ["APP_BASE_URL"]      # Keys to set as non-secret vars (optional)
environment = "staging"          # Optional wrangler environment
deploy = "wrangler deploy"       # Custom deploy command (optional)
dev = "wrangler dev"             # Custom dev command (optional)
```

### Setup (TUI)

```bash
# Interactive Cloudflare setup (detects wrangler config + env files)
f deploy setup
```

### Deploy

```bash
# Deploy to production
f deploy cf

# Set secrets and deploy
f deploy cf --secrets

# Run in dev mode
f deploy cf --dev
```

### Secrets

If you specify `env_file`, flow will set each variable as a Cloudflare secret:

```env
# .env.cloudflare
API_KEY=secret123
DATABASE_URL=postgres://...
```

```bash
f deploy cf --secrets
# Sets API_KEY and DATABASE_URL via `wrangler secret put`
```

If you set `env_source = "1focus"`, flow will fetch env vars from 1focus instead of a local file:

```bash
f env apply
```

If `environment` is set, Flow appends `--env <name>` for secrets and deploys.

## Railway

### Prerequisites

- Railway CLI: `npm install -g @railway/cli`
- Authenticated: `railway login`

### Configuration

```toml
[railway]
project = "your-project-id"      # Railway project ID
environment = "production"       # Environment name
env_file = ".env.railway"        # Environment variables
```

### Deploy

```bash
f deploy railway
```

## Examples

### Rust Server

```toml
[host]
dest = "/opt/api"
setup = """
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
cargo build --release
"""
run = "/opt/api/target/release/server"
port = 8080
service = "api-server"
env_file = ".env.production"
domain = "api.example.com"
ssl = true
```

### Node.js App

```toml
[host]
dest = "/opt/webapp"
setup = """
curl -fsSL https://deb.nodesource.com/setup_20.x | bash -
apt-get install -y nodejs
npm ci --production
npm run build
"""
run = "node /opt/webapp/dist/server.js"
port = 3000
service = "webapp"
env_file = ".env"
domain = "app.example.com"
ssl = true
```

### Python/FastAPI

```toml
[host]
dest = "/opt/api"
setup = """
apt-get install -y python3 python3-pip python3-venv
python3 -m venv venv
./venv/bin/pip install -r requirements.txt
"""
run = "/opt/api/venv/bin/uvicorn main:app --host 0.0.0.0 --port 8000"
port = 8000
service = "fastapi"
env_file = ".env"
```

### Cloudflare Worker with Hono

```toml
[cloudflare]
path = "worker"
env_file = ".env.cf"
```

```bash
# In worker/ directory, have wrangler.toml:
# name = "my-api"
# main = "src/index.ts"

f deploy cf --secrets
```

## Tips

### Multiple Environments

Use different env files for staging vs production:

```bash
# Deploy to staging
FLOW_ENV=staging f deploy

# Or use different flow.toml sections (coming soon)
```

### CI/CD Integration

```yaml
# GitHub Actions
- name: Deploy
  run: |
    echo "${{ secrets.DEPLOY_KEY }}" > ~/.ssh/id_ed25519
    chmod 600 ~/.ssh/id_ed25519
    f deploy set-host ${{ secrets.DEPLOY_HOST }}
    f deploy
```

### Viewing Deployed Service

```bash
# Check status
f deploy status

# View logs
f deploy logs -n 200

# Follow logs
f deploy logs -f

# SSH in for debugging
f deploy shell
```

### Rollback

Currently manual - SSH in and use git:

```bash
f deploy shell
cd /opt/myapp
git checkout HEAD~1
systemctl restart myapp
```

## Troubleshooting

### "No host configured"

```bash
f deploy set-host user@host:port
```

### "Permission denied"

Ensure SSH key is set up:
```bash
ssh-copy-id user@host
```

### "nginx: command not found"

Install nginx on the server:
```bash
f deploy shell
apt-get install -y nginx
```

### "certbot: command not found"

Install certbot for SSL:
```bash
f deploy shell
apt-get install -y certbot python3-certbot-nginx
```

### Service won't start

Check logs:
```bash
f deploy logs
f deploy shell
journalctl -u myservice -e
```
