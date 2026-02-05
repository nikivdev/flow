# Assistant Rules (flow)

Only load skills when the request clearly needs them.

## Skills (on-demand)
- flow-native: Use for Flow CLI native workflows (env setup, secrets, deploys, logs, deps), for repos with `flow.toml`, or Cloudflare Workers. Avoid direct pnpm/wrangler unless asked.
- flow-interactive: Use when commands are interactive or could block on stdin (e.g., `f setup`).
- flow-dev-traces: Use when debugging Flow proxy behavior, tracing requests, or when the user asks about proxyx, trace-summary.json, or flow trace commands.
- flow-usage: Use when running or troubleshooting Flow command behavior.
- internal-ai-inference: Use only when asked to run inference or integrate with internal AI tooling.

Default: Avoid loading skills for routine edits, reviews, or simple questions.
