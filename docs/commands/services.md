# f services

Guided setup flows for third-party services. These commands prompt for required
env vars, store them in 1focus, and can optionally apply them to Cloudflare.

## Stripe

```bash
f services stripe
```

### Options

- `--path <PATH>`: target project root (defaults to current directory).
- `--environment <ENV>`: env store to write (default: flow.toml or `production`).
- `--mode <test|live>`: Stripe mode (default: `test`).
- `--force`: prompt even if keys are already set.
- `--apply` / `--no-apply`: apply envs to Cloudflare after setup.

### What it prompts for

The command inspects `flow.toml` `[cloudflare].env_keys` and asks for Stripe
keys found there (fallback order):

- `STRIPE_SECRET_KEY`
- `STRIPE_WEBHOOK_SECRET`
- `STRIPE_PRO_PRICE_ID`
- `STRIPE_REFILL_PRICE_ID`
- `VITE_STRIPE_PUBLISHABLE_KEY`

### Helpful Stripe sources

- Secret/Publishable keys: Stripe Dashboard -> Developers -> API keys
- Webhook signing secret: Stripe Dashboard -> Developers -> Webhooks (or `stripe listen --print-secret`)
- Price IDs: Stripe Dashboard -> Products -> Price (starts with `price_`)

### Example

```bash
cd /Users/nikiv/org/gen/new
f services stripe --mode test --apply
```
