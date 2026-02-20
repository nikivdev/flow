# Rise

This document explains how Rise integrates with Flow and why installing Rise gives you a high-leverage workflow layer across repos.

Rise itself is treated as closed/internal product code in many environments, but its operator model and command surface can still be documented and shared.

## Start Here

Integration starts with one command:

```bash
f install rise
```

After install:

```bash
which rise
rise --help
```

You now have `rise` on your PATH and can use Rise workflows from any repo.

From Flow's install behavior (`f install`):

- backend resolution is automatic (`registry` -> `parm` -> `flox`)
- `rise` is a built-in known install target
- Rise can be part of bootstrap tool install flows

## What You Get From `rise` In Practice

Rise is not just one command; it is a workflow layer that adds:

1. Repo adoption overlays for external/team repositories.
2. Task detection and `flow.toml` generation/merge.
3. JJ-native branch/bookmark workflow for clean PRs.
4. Multi-platform compile/dev loops (web, Expo/mobile, Electron, COI).
5. Mobile/TestFlight build observability and debugging.
6. Schema workflows with generated TypeScript/Effect bindings.
7. Sandbox verification in VM environments.
8. AI and trace workflows that integrate with Flow and surrounding tooling.

## Core Model: Overlay, Not Pollution

The defining Rise behavior is `rise adopt`.

For external repos, Rise creates a JJ overlay bookmark (`rise`) above `main`:

- `rise` layer contains your local workflow files (for example `flow.toml`, `.rise/`).
- Team-facing `main` remains untouched.
- PR branches are created from `main`, so Rise files do not leak into PRs.

Typical flow:

```bash
rise adopt https://github.com/org/repo
cd ~/code/org/repo
rise sync
jj new main -m "feat: clean PR change"
```

This is the main reason Rise is valuable in mixed team environments: private operator ergonomics without contaminating shared repo history.

## Task Detection And Flow Integration

During adoption, Rise detects project tasks from common sources (for example `package.json`, `Makefile`, `Cargo.toml`, `go.mod`, `pyproject.toml`, `justfile`) and generates `flow.toml`.

You keep the generated baseline and extend it with your own project-specific tasks.

Useful commands:

```bash
rise adopt .
rise adopt --force .
rise flow tasks .
rise sync
rise list
```

Key integration point: this makes Flow task execution (`f <task>`) available even in repos that did not originally ship with Flow conventions.

## Development Loops

Rise provides higher-level wrappers while preserving underlying toolchains.

Common commands:

```bash
rise dev
rise app
rise run dev --runner turbo
rise setup
rise verify
```

From Rise docs/repo behavior:

- `rise dev` starts local dev paths with platform-aware behavior.
- `rise app` is a fast app shortcut path.
- `rise run` can delegate to task runners (Turbo/Flow patterns).
- `rise setup` is project setup entrypoint.
- `rise verify` is local verification flow.

## Mobile/TestFlight Workflows (High Value)

Rise has dedicated mobile commands with structured observability:

```bash
rise mobile validate
rise mobile preflight
rise mobile testflight
rise mobile builds
rise mobile logs
```

Why this matters:

- `validate` catches JS bundle issues quickly.
- `preflight` checks config + bundle + prebuild inspect.
- `testflight` captures structured build events.
- `builds`/`logs` provide post-failure visibility and faster debugging loops.

This reduces the "wait 10+ minutes to discover obvious build issues" pattern in Expo/EAS flows.

## Schema Workflows

Rise includes schema lifecycle commands:

```bash
rise schema init
rise schema status
rise schema diff
rise schema generate
rise schema push
rise schema validate
```

This supports a source-of-truth schema flow with generated app bindings (including TypeScript/Effect-oriented outputs in documented workflows).

## Sandbox Workflows

Rise supports VM-backed sandbox execution and verification:

```bash
rise sandbox
rise sandbox verify
rise verify --sandbox
rise sandbox kill
rise sandbox clean
```

Use this when you need stronger isolation for verification or reproduction loops.

## AI + Traces + Operational Debugging

Rise docs also describe:

- AI trace collection (`rise logs`)
- build failure context for AI-assisted remediation (`rise work --errors` paths)
- integration patterns around generated prompts and operational context

For Flow users, this complements:

- `f commit` review/message provider strategies that can call Rise-backed providers
- task failure hooks that invoke `rise work`

## Typical Onboarding Sequence

For a new machine:

```bash
f doctor
f auth login
f install rise
rise --help
```

For an external/team repo:

```bash
cd ~/code/org/repo
rise adopt .
rise sync
```

For daily work:

```bash
rise verify
rise mobile preflight      # if mobile repo
jj new main -m "feat: ..."
```

## When To Use Rise

Use Rise when you want:

- an opinionated operator layer over heterogeneous repos
- clean PRs with private local tooling overlays
- stronger mobile/testflight diagnostics
- schema/sandbox/dev orchestration from one CLI surface

Use plain Flow-only setup when:

- repo already has stable native workflows and minimal onboarding cost
- you do not need overlay-based separation
- your team explicitly avoids JJ overlay workflows

## Relationship To Flow

Flow and Rise are complementary:

- Flow is the general control plane (`f tasks`, `f env`, `f commit`, deploy/sync/invariants).
- Rise is the repo/workflow acceleration layer for adoption, platform compile loops, mobile observability, and overlay workflows.

The practical entrypoint is still:

```bash
f install rise
```

Then use `rise` where it adds leverage, while keeping Flow as your consistent command contract across projects.

## References In Rise Docs

If you have access to the internal Rise repo docs, these are particularly relevant:

- `docs/adopt-guide.md`
- `docs/workflow-guide.md`
- `docs/rise-branch-workflow.md`
- `docs/build-observability.md`
- `docs/schema-guide.md`
- `docs/sandbox-vibe.md`
- `docs/rise-mobile-compat.md`
- `docs/expo-identifiers.md`
