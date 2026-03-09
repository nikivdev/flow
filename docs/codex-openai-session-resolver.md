# Codex OpenAI Repo Session Resolver

This documents the personal `L` wrapper that opens or resumes Codex sessions with repo-specific query matching for `~/repos/openai/codex`.

Relevant files:

- fish entrypoint: `/Users/nikitavoloboev/config/fish/fn.fish`
- resolver: `/Users/nikitavoloboev/config/fish/scripts/codex-openai-session.ts`

## Behavior

- `L` with no args runs `f ai codex new`.
- `L <query>` targets stored Codex sessions for `~/repos/openai/codex`.
- On a successful match it runs `f ai codex resume <thread-id>` in that repo.
- On no match it exits non-zero and prints a short recent-session list instead of opening the wrong conversation.

## Why use Codex app-server

This path uses `codex app-server` instead of parsing `f ai codex list` output.

That matters because `thread/list` gives:

- exact `cwd` filtering for `~/repos/openai/codex`
- stable `updatedAt` ordering
- structured fields such as `id`, `name`, `preview`, `gitInfo`, and `cwd`
- pagination and optional server-side `searchTerm`

For this wrapper, exact repo scoping is the main win. It avoids mixing sessions from unrelated repos and avoids depending on Flow's imported session index.

## Resolution flow

1. Spawn `codex app-server` with cwd set to `~/repos/openai/codex`.
2. Send `initialize`, then `initialized`.
3. Call `thread/list` with:
   - `cwd: /Users/nikitavoloboev/repos/openai/codex`
   - `archived: false`
   - `sortKey: updated_at`
4. Use a small first fetch when possible:
   - latest query: 1
   - `after most recent active`: 2
   - text search query: 25
   - fallback scan: up to 100
5. Resolve the query against returned threads.
6. For textual queries, rerank the top shortlist with `thread/read includeTurns:true` using full turn text.
   - if summary fields miss completely, probe the most recent few threads by full turn text before giving up
7. Resume the chosen thread through `f ai codex resume <id>` in the Codex repo.

## Query matching rules

The resolver is deterministic. It does not call a model.

Matching order:

1. exact or unique thread id prefix
2. relative query with `after` or `before`
3. ordinal query such as `2`, `second`, `3rd`
4. text ranking across:
   - `thread.name`
   - `thread.preview`
   - `thread.gitInfo.branch`
   - `thread.gitInfo.sha`
5. pure recency query such as `most recent active`

Examples:

- `L most recent active`
- `L session after most recent active`
- `L second`
- `L 019cca91`
- `L where does codex store`
- `L history.jsonl`

Important accuracy guardrails:

- `last` only means "latest session" when the rest of the query is otherwise empty after control words are removed.
- bare numbers only become ordinals when the query reduces to just that number.
- directional queries stay directional. If there is no next or previous match, the resolver fails instead of silently falling back to the latest session.

## Current limits

- It starts a fresh `codex app-server` process on every lookup. That is the main latency cost.
- `thread/list searchTerm` only filters extracted titles and is case-sensitive, so the helper still needs local fallback ranking.
- The second pass only reads a few candidate threads, so this is still a bounded heuristic rather than a full semantic search across all history.
- Relative anchor queries are strongest for latest or ordinal anchors and weaker for arbitrary natural-language anchors.

## Best improvements

### Speed

- Keep a long-lived local resolver daemon so `L` reuses one app-server connection instead of spawning a fresh process.
- Add a small local cache keyed by repo path with `id`, `updatedAt`, `name`, `preview`, and `gitInfo`, then refresh it opportunistically.
- If Codex exposes a stable reusable host transport beyond stdio for local clients, switch to that instead of process-per-query.

### Accuracy

- Start naming important sessions with `thread/name/set`; exact names will beat fuzzy preview matching.
- If the wrapper ever controls session creation, persist higher-signal naming or metadata early instead of inferring from preview text later.
- Extend the second pass to handle arbitrary textual anchors inside `after ...` and `before ...` queries more deeply when the anchor match is still weak.

### Prompting

A model-based resolver is possible but should be the fallback, not the first pass.

Why:

- slower than deterministic matching
- easier to silently choose the wrong session
- unnecessary when `id`, `updatedAt`, `name`, `preview`, and repo path already narrow the space well

If a model is added later, the safe shape is:

- deterministic shortlist first
- prompt only over the top few candidates
- require the model to return one thread id or `NONE`
- keep strict failure if confidence is weak

## Best next step

The highest-value next change is reducing lookup latency:

1. keep one local long-lived resolver or daemon
2. reuse one app-server connection per repo
3. cache recent shortlist results and refresh opportunistically

That should improve the user-visible speed more than further prompt tuning.
