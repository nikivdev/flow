# Codex-First Control Plane Roadmap

This document proposes how Flow should evolve from "helpful CLI + local skills"
into a Codex-first control plane where the user stays inside Codex and Flow
handles routing, memory, execution, and learning behind the scenes.

## Goal

Target state:

- the user speaks natural intent in Codex
- Flow resolves references, routes workflows, fetches secure context, and runs
  the right tool/task
- Codex sees only the smallest useful context for the current turn
- repeated phrasing becomes reusable system knowledge without turning every repo
  preamble into a wall of rules

Example desired behavior:

- `document it` resolves to the docs write flow
- a pasted Linear URL is unrolled before planning
- `continue the last deploy investigation` finds the right session/worktree
- the user does not need to remember `forge doc`, `forge linear inspect`, or
  repo-specific wrappers

## Problem

Current Flow has strong building blocks but they are still separate:

- task skills are generated and reloaded for Codex
- sessions are stored and recoverable
- env storage is becoming secure enough for org use
- router telemetry already exists
- repo-specific systems like Forge can mine aliases and inject lean workflow
  rules

But the user still pays too much cognitive cost:

- wrappers like `L` and repo-specific launchers carry logic outside Flow
- repo preambles grow whenever a new shortcut is taught
- skill learning is mostly manual
- URL/reference unrolling is repo-specific instead of generic
- Codex app-server connections are process-per-query in some paths

The result is "good pieces, weak control plane".

## Design Principles

1. Flow is the control plane; repo tools remain domain executors.
2. Skills stay thin; runtime resolution carries the real behavior.
3. Reference unrolling is deterministic first, model-assisted only if needed.
4. Learning produces suggestions, not prompt bloat.
5. No default context should be paid for behavior that is not active.

## Existing Flow Building Blocks

- task-synced Codex skill metadata in [src/skills.rs](../src/skills.rs#L378) and [src/skills.rs](../src/skills.rs#L443)
- Codex skill cache reload in [src/skills.rs](../src/skills.rs#L1224)
- configurable Codex wrapper transport in [src/commit.rs](../src/commit.rs#L5414)
- multi-provider session recovery and copy flows in [src/ai.rs](../src/ai.rs#L1)
- router telemetry hooks in [src/rl_signals.rs](../src/rl_signals.rs#L307)
- current Codex session resolver direction in [codex-openai-session-resolver.md](codex-openai-session-resolver.md#L1)

These are enough to start. The missing work is unification.

## Constraint: no Codex fork

Flow should target current upstream Codex directly.

That means:

- prefer wrapper transport + config over patching Codex
- use stable upstream surfaces like normal user skill roots, `skills/list`, and `thread/*`
- treat newer upstream features such as `skills/list perCwdExtraUserRoots` and in-process app-server clients as accelerators, not prerequisites
- keep repo-specific behavior in Flow or repo executors, not in a private Codex fork

## Proposed Architecture

### 1. warm Codex control layer

Add a Flow-managed warm control layer, either as an extension of `ai-taskd`, a
focused `codexd`, or a lighter in-process broker where that is enough for the
current upstream Codex client surface.

Responsibilities:

- maintain repo-scoped Codex app-server sessions
- cache recent threads, active skills, and repo metadata
- expose fast local RPC for lookup, runtime-skill injection, and doctor output
- resolve references before they reach Codex as plain text
- own the "what extra context is actually needed for this turn?" decision

This should absorb behavior that currently lives in wrappers like `L`.

### 2. Intent registry

Promote Forge-style phrase aliasing into Flow as a generic feature.

Each intent has:

- canonical name
- phrase aliases
- optional repo/path scope
- resolver/action target
- confidence policy
- evidence counters for suggested future aliases

Examples:

- `doc-it`
- `linear-reference`
- `session-recover`
- `review-intent-comment`

Intent matching must stay deterministic and cheap.

### 3. Reference resolvers

Flow should ship a generic resolver layer for pasted references:

- Linear issue URLs
- Linear project URLs
- GitHub PR / issue URLs
- repo file paths
- commit SHAs
- saved Flow session names or IDs

Resolvers return structured payloads, not prose. Repo-local executors like
Forge can register resolver commands for domain-specific expansion.

### 4. Runtime skills

Split Codex knowledge into two layers:

- baseline skills: always available, minimal repo guidance
- runtime skills: ephemeral, injected only when a matched intent or resolver
  requires them

Examples:

- user says `document it`
  - inject tiny docs-routing runtime skill
- user pastes a Linear URL
  - inject tiny linear-unrolled runtime context
- user asks to recover recent work
  - inject session-recovery runtime context only for that request

Runtime skills should expire automatically and be bounded by a strict budget.

### 5. Suggestion loop, not self-bloating memory

Use router telemetry plus transcript mining to propose:

- new aliases
- new reference patterns
- candidate runtime skills
- stale skills that should be removed

Important:

- do not auto-install every observed phrase
- require evidence thresholds
- prefer suggested changes that collapse multiple variants into one canonical
  intent

## Flow Commands

Add a small command family around the new control plane:

```bash
f codex open [query]
f codex resolve "<text-or-url>" [--json]
f codex runtime
f codex runtime show
f codex runtime clear
f codex teach suggest
f codex teach accept <intent-or-suggestion-id>
f codex teach reject <intent-or-suggestion-id>
f codex doctor
f codexd start|stop|status
```

Intended behavior:

- `f codex open` replaces personal wrappers like `L`
- `f codex resolve` shows what Flow would unroll or route before Codex sees it
- `f codex runtime show` explains which runtime skills/context are active
- `f codex teach suggest` presents evidence-backed alias/intent suggestions
- `f codex doctor` exposes repo path, active app-server connection, runtime
  budget, skill count, and recent resolver hits

## Config Shape

Proposed `flow.toml` additions:

```toml
[codex]
control_plane = "daemon"
warm_app_server = true
runtime_skill_budget_chars = 1200
auto_resolve_references = true
auto_learn = "suggest-only"

[codex.session]
open_command = "codex"
prefer_last_active = true
repo_scoped_lookup = true

[[codex.intent]]
name = "doc-it"
phrases = ["doc it", "document it", "write this down", "save this in docs"]
resolver = "docs.route_write"
scope = ["repo", "personal"]

[[codex.intent]]
name = "session-recover"
phrases = ["what was i doing", "recover recent context", "continue the work"]
resolver = "session.recover"

[[codex.reference_resolver]]
name = "linear"
match = ["https://linear.app/*/issue/*", "https://linear.app/*/project/*"]
command = "forge linear inspect {{ref}} --json"
inject_as = "linear"

[[codex.reference_resolver]]
name = "docs"
match = ["doc it", "document it"]
command = "forge doc route --title {{title}} --json"
inject_as = "docs"
```

Also add a personal/global config file for user-specific phrase preferences:

- `~/.config/flow/codex-intents.toml`

Use this for personal language variants that should not live in repo config.

## Daemon Responsibilities

`codexd` should own:

- app-server lifecycle
- repo session caches
- runtime skill activation/deactivation
- resolver execution
- secure env lookups for active workflows
- bounded prompt-context assembly
- suggestion generation from telemetry/history
- compatibility with existing `f skills reload` and `f ai codex ...` flows

It should not:

- replace repo-specific executors like Forge
- run opaque model-based routing in the hot path
- inject large transcript summaries into every turn

## Prompt Budget Policy

The runtime layer needs hard limits:

- baseline repo guidance stays small
- runtime additions must fit a bounded char/token budget
- each resolved intent/reference should justify its own inclusion
- unused runtime skills expire quickly

Budget policy should prefer:

1. structured resolver output
2. one tiny runtime skill
3. one short recovery summary
4. nothing else

## Learning Loop

Inputs:

- router telemetry
- accepted/overridden task choices
- resolver hits
- successful tool invocations
- session transcript mining

Outputs:

- proposed alias additions
- proposed resolver registrations
- dead-skill cleanup suggestions
- better default repo baselines

Approval model:

- repo suggestions require explicit accept
- personal suggestions can default to personal scope
- org/shared suggestions should stay gated

## Relationship To Forge

Forge should remain the Prom executor for Prom-specific workflows.

Flow should absorb the generic pieces Forge proved useful:

- intent aliasing
- reference unrolling
- thin runtime teaching
- lean docs workflow activation

That means:

- Prom keeps `forge linear inspect`, `forge doc`, and similar domain commands
- Flow becomes the generic router that decides when to call them

## Rollout Phases

### Phase 0: unify wrappers

- move `L`-style session open/recover behavior into `f codex open`
- make repo-scoped Codex session resolution first-class
- expose a `doctor` view for current skill/runtime state

### Phase 1: warm daemon

- add `codexd` with persistent app-server connection per repo
- keep recent thread cache and skills cache warm
- remove process-per-query overhead for session lookup/reload paths

### Phase 2: intent registry + resolvers

- add config-backed intent aliases
- add generic reference resolver interface
- ship built-ins for session recovery, docs routing, and Linear URLs

### Phase 3: runtime skills

- inject temporary runtime skills/context instead of growing repo preambles
- enforce runtime budget caps
- surface active runtime state in `f codex runtime show`

### Phase 4: learning loop

- mine telemetry + sessions for candidate aliases and resolver patterns
- generate suggestions only after evidence thresholds
- add accept/reject workflow

### Phase 5: provider expansion

- reuse the same intent/resolver plane for Claude and Cursor transcript-backed
  workflows where useful
- keep Codex as the first-class interactive target

## First Implementation Slice

The highest-value first slice is:

1. `f codex open`
2. `codexd` with warm repo-scoped app-server
3. `f codex resolve`
4. config-backed intents
5. built-in resolvers for:
   - docs intents
   - Linear URLs
   - session recovery prompts
6. `f codex runtime show`

Why this first:

- it removes the most command-memory burden immediately
- it uses Flow’s existing app-server + skills + session foundations
- it keeps the prompt surface thin
- it gives a concrete place to move personal wrapper logic

## Success Metrics

- p50 `f codex open` latency
- number of user prompts that required remembering a repo command
- average runtime-context bytes injected per turn
- resolver hit rate
- accepted suggestion rate
- count of active baseline skills versus runtime skills

## Non-Goals

- full semantic agent routing in the hot path
- unbounded transcript mining into prompt context
- replacing repo executors with Flow clones
- auto-learning every phrase without evidence or approval

## Summary

The target system is not "more AGENTS text" and not "more commands for the
user to remember".

It is:

- thin baseline repo guidance
- a warm Flow Codex control daemon
- deterministic intent/reference resolution
- ephemeral runtime skills
- evidence-backed learning with approval

That is how Flow becomes truly Codex-first while keeping context cost low.
