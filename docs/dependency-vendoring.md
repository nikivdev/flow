# Dependency Vendoring (Cargo-First)

This project uses a Cargo-first vendoring model:

- Cargo remains the resolver, lockfile authority, and build system.
- Vendored source is owned in `nikivdev/flow-vendor`.
- `flow` pins vendored state by commit in `vendor.lock.toml`.
- `flow` uses `[patch.crates-io]` path overrides into `lib/vendor/*`.

This gives direct dependency control without giving up Cargo behavior.

## Why This Model

### Problem

- crates.io + transitive dependency growth hurts compile times and iteration speed.
- upstream crates can pull convenience dependencies, macros, and features we do not need.
- editing third-party code in-place inside the main repo pollutes history and makes updates hard.

### Requirements

- keep Cargo benefits (resolver correctness, lock semantics, ecosystem compatibility),
- gain direct control over dependency source and shape,
- keep upstream sync fast and automatable,
- keep repository history readable.

### Result

- dependency source churn lives in `flow-vendor`,
- application-level pin and wiring lives in `flow`,
- updates are reproducible and lock-pinned,
- trim/refactor opportunities are local and fast.

## Nix-Inspired Discipline

This model borrows the parts of Nix that matter most for dependency control:

- pinned inputs (`vendor.lock.toml`, `Cargo.lock`),
- deterministic materialization (`vendor-repo.sh hydrate`),
- provenance/checksum verification (`vendor-manifest`, strict verify),
- transactional updates with rollback safety (`vendor-control.sh inhouse`),
- closure-size reduction by trimming unused dependency surface.

Reference:

- `docs/vendor-nix-inspiration.md`

## Benefits

- Faster local iteration by removing unneeded dependency surface area.
- Ability to aggressively trim crates to exactly what `flow` uses.
- Deterministic hydration in CI and local environments from a pinned vendor commit.
- Clean `flow` history: metadata/pins in `flow`, source churn in `flow-vendor`.
- Upstream updates remain scriptable and reviewable.

## Core Files and Their Roles

- `vendor.lock.toml`
  - Source of truth for vendor remote, branch, checkout, pinned commit, and crate map.
- `Cargo.toml`
  - `[patch.crates-io]` points selected crates to `lib/vendor/<crate>`.
- `Cargo.lock`
  - Must resolve vendored crates by path (no registry source for vendored entries).
- `lib/vendor/<crate>`
  - Materialized source tree used by Cargo path patches.
- `lib/vendor-manifest/<crate>.toml`
  - Per-crate metadata for version/provenance/sync and verification.
- `scripts/vendor/*`
  - Toolkit for inhouse, hydrate, status, sync, and vendor-repo operations.

## Repositories

### `flow` repo

- owns pins, manifests, trim logic hooks, and Cargo wiring.
- should not include full vendored source history churn.

### `flow-vendor` repo

- canonical storage for vendored crate source (`crates/<crate>`),
- vendored crate manifests (`manifests/<crate>.toml`),
- profile metadata used during hydration.

## Operating Principle: Cargo First

Do not replace Cargo. Use Cargo as the system of record:

- resolve versions through `Cargo.lock`,
- use `cargo update -p <crate> --precise <version>` for deterministic lock rewrites,
- build and validate with normal Cargo commands (`cargo check`, `cargo test --no-run`),
- use vendoring only as controlled source substitution via patches.

## Standard Workflow (One Crate)

Recommended entrypoint:

```bash
/Users/nikiv/code/rise/scripts/vendor-control.sh inhouse --project /Users/nikiv/code/flow <crate> [version]
```

What this does:

1. Ensures lock entry and Cargo patch wiring.
2. Materializes crate from Cargo cache into `lib/vendor/<crate>`.
3. Stores crate history in `lib/vendor-history/<crate>.git`.
4. Writes `lib/vendor-manifest/<crate>.toml` + `UPSTREAM.toml`.
5. Re-syncs `Cargo.lock` to exact vendored version.
6. Applies trim hooks (`scripts/vendor/apply-trims.sh`).
7. Imports local materialized source into `.vendor/flow-vendor`.
8. Pins `vendor.lock.toml` to new vendor commit.

## Verification and Safety Gates

Run after each vendoring step:

```bash
/Users/nikiv/code/rise/scripts/vendor-control.sh verify --project /Users/nikiv/code/flow
cargo check -q
scripts/vendor/sync-all.sh --important --dry-run
```

`verify` enforces:

- crate exists in `vendor.lock.toml`,
- crate exists in `Cargo.lock`,
- no registry source for vendored crate in `Cargo.lock`,
- one resolved version per vendored crate,
- patch path matches lock materialized path,
- manifest version matches lock version.

## Provenance and Hardening

`inhouse` now records provenance fields in crate manifests:

- `registry_index`
- `cargo_registry_checksum`
- `crate_archive_sha256`
- `checksum_match`
- `upstream_repository`
- `upstream_homepage`
- `history_head`

Use report mode:

```bash
/Users/nikiv/code/rise/scripts/vendor-control.sh provenance --project /Users/nikiv/code/flow
```

Use stricter mode when migrating fully:

```bash
/Users/nikiv/code/rise/scripts/vendor-control.sh verify --project /Users/nikiv/code/flow --strict-provenance
```

## Transactional Failure Behavior

`vendor-control.sh inhouse` includes rollback protection by default:

- snapshot relevant files before mutation,
- on failure, restore pre-run `Cargo.toml`, `Cargo.lock`, `vendor.lock.toml`,
- remove newly created manifest/source/history artifacts for failed crate,
- restore prior vendor lock pin.

Escape hatch (not recommended except debugging):

```bash
/Users/nikiv/code/rise/scripts/vendor-control.sh inhouse --project /Users/nikiv/code/flow <crate> --no-rollback
```

## Upstream Sync Loop

Track updates:

```bash
scripts/vendor/check-upstream.sh --important
scripts/vendor/sync-all.sh --important --dry-run
```

Apply updates intentionally:

```bash
scripts/vendor/sync-all.sh --important
scripts/vendor/vendor-repo.sh import-local
git -C .vendor/flow-vendor push origin main
```

Policy:

- patch updates can be frequent,
- minor/major updates happen in explicit review windows (`--allow-minor`, `--allow-major`).

## Code Intelligence Loop (opensrc-style, crates-focused)

To make vendored code practical at scale, we index first-party + vendored sources into
Typesense and query them fast during refactors and trim work.

This follows the same high-level pattern as `opensrc`:

- keep a local source inventory (`.vendor/typesense/sources.json`),
- keep local source materialized (already done by vendor hydrate/inhouse),
- index/search against local code state, not remote assumptions.

Flow entrypoints:

```bash
f vendor-typesense-setup   # one-time if Typesense is not installed locally
f vendor-typesense-up
f vendor-code-index
f vendor-code-search "Router"
f vendor-code-search "serde" --scope vendor --crate axum
f vendor-code-search-sources "ratatui"
```

Script used by tasks:

```bash
python3 ./scripts/vendor/typesense_code_index.py --help
```

Design goals:

- search by vendored crate boundary (`--crate <name>`),
- search by ownership boundary (`--scope vendor|firstparty`),
- keep source provenance in inventory (`version`, `checksum`, `history_head`),
- make trim/upstream update work faster by removing "where is this code?" overhead.

Reference:

- `docs/vendor-code-intelligence.md` for architecture, commands, and operating loop.

## CI Contract

CI must hydrate vendored source from `vendor.lock.toml` before Cargo build:

```bash
scripts/vendor/vendor-repo.sh hydrate
```

Any CI build skipping hydrate can fail with missing `lib/vendor/*` path deps.

## Optimization Strategy (Compile-Time Focus)

For each vendored crate:

1. inspect real usage in `flow` (APIs/types called),
2. remove optional features not used,
3. delete convenience-only dependencies,
4. remove proc-macro convenience layers where reasonable,
5. reduce duplicate major versions where possible,
6. keep trim hooks deterministic and replayable.

Use:

```bash
scripts/vendor/offenders.sh
cargo tree -d
```

to rank impact and watch duplicate-version pressure.

Operational tooling for this loop:

- `f vendor-rough-audit`
- `f vendor-offenders`
- `f vendor-bench-iter -- --mode incremental --samples 3`
- `f vendor-optimize-loop`

Reference:

- `docs/vendor-optimization-loop.md`

## Commit Policy

- In `flow`: commit only lock/manifest/patch/docs/script changes.
- In `flow-vendor`: commit source churn.
- Push `flow-vendor` first, then push `flow` pin updates.
- Prefer one crate per commit for auditability.

## Recovery Playbook

Inspect state:

```bash
scripts/vendor/vendor-repo.sh status
```

Re-hydrate local materialization from pinned commit:

```bash
scripts/vendor/vendor-repo.sh hydrate
```

Re-pin to known commit:

```bash
scripts/vendor/vendor-repo.sh pin <commit>
```

## FAQ

### Are we replacing Cargo?

No. Cargo remains central. Vendoring is an ownership layer on top.

### Why separate repo for vendored source?

To keep main repo history focused on product changes while retaining full dependency source control.

### Can we still pull upstream changes quickly?

Yes. `check-upstream` + `sync-*` + locked import flow is designed for repeatable upstream ingestion.
