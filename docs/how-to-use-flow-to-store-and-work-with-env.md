# How to Use Flow Env Store (Project + Personal)

This is the context-optimized workflow for current Flow behavior.

## Important Scope Rule

- `f env set KEY=VALUE` writes to **personal** env scope.
- `f env project set KEY=VALUE` writes to **project** env scope.

If you need deploy/runtime envs for a project, use `f env project ...`.

## Backend Choice

Flow supports:

- `cloud` (myflow.sh): shared/team-friendly.
- `local` (`~/.config/flow/env-local/`): offline/no-account.

Set backend in `~/.config/flow/config.ts`:

```ts
export default {
  flow: { env: { backend: "cloud" } } // or "local"
}
```

Or per shell:

```bash
export FLOW_ENV_BACKEND=local
```

## Fast Path (Project Env, Production)

```bash
# 1) Login once if using cloud
f env login

# 2) Set project env vars (not personal)
f env project set DATABASE_URL=postgres://... -e production
f env project set RESEND_API_KEY=re_... -e production

# 3) Verify (masked)
f env project list -e production

# 4) Run app with injected project envs
f env run -e production -- pnpm start

# 5) Optional: write current project envs to local .env
f env pull -e production
```

## Personal Tokens (User Scope)

Use for developer-specific tokens (GitHub, CLI auth, etc.):

```bash
f env set GITHUB_TOKEN=ghp_...
f env get --personal GITHUB_TOKEN -f value
f env run --personal -k GITHUB_TOKEN -- gh auth status
```

Deepgram example (keep value in Flow store, never in docs):

```bash
# Set once (personal scope)
f env set --personal DEEPGRAM_API_KEY=<redacted>

# Read when needed
f env get --personal DEEPGRAM_API_KEY -f value
```

## Environment Names

Supported environments:

- `production` (default)
- `staging`
- `dev`

Example:

```bash
f env project set API_URL=https://staging.example.com -e staging
f env run -e staging -- pnpm dev
```

## Guided Flows

Use these when `flow.toml` already declares required keys:

```bash
f env keys
f env guide -e production
f env setup
```

## Deploy Integration

### Cloudflare Workers

In `flow.toml`:

```toml
[cloudflare]
env_source = "cloud" # or "local" for local backend reads
env_keys = ["DATABASE_URL", "BETTER_AUTH_SECRET", "APP_BASE_URL"]
env_vars = ["APP_BASE_URL"] # non-secret vars
environment = "production"
```

Apply:

```bash
f env apply
```

### Host Deploys

In `flow.toml`:

```toml
[host]
env_source = "flow" # or "local"
env_keys = ["DATABASE_URL", "RESEND_API_KEY"]
environment = "production"
```

Then:

```bash
f deploy
```

## Local Backend Storage Layout

When backend is `local`, Flow writes plain `.env` files:

```
~/.config/flow/env-local/
├── <project-or-space>/
│   ├── production.env
│   ├── staging.env
│   └── dev.env
└── personal/
    └── production.env
```

These files are not encrypted.

## Quick Troubleshooting

- "Not logged in": run `f env login` (or force local backend).
- Values not found: check environment (`-e staging` vs `production`).
- Command injection not working: keep `--` before command.
  - Correct: `f env run -k API_KEY -- node app.js`
  - Wrong: `f env run -k API_KEY node app.js`
