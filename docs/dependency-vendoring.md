# Dependency Vendoring

Flow now uses a split model:

- `nikivdev/flow-vendor` is the canonical vendored source repo.
- `flow` pins an exact vendor commit in `vendor.lock.toml`.
- `lib/vendor/*` is local materialized source for Cargo path patches.

## Goals

- Full dependency ownership and aggressive trim capability.
- Fast iteration in `flow` without polluting `flow` history with vendored source churn.
- Deterministic, lock-pinned hydration from vendor source.

## Core Files

- `vendor.lock.toml`: vendor repo URL/branch/checkout path, pinned commit, crate mapping.
- `scripts/vendor/vendor-repo.sh`: lifecycle orchestration (init/import/hydrate/pin/status/push).
- `lib/vendor-manifest/*.toml`: crate metadata used for update checks.

## Vendor Repo Layout (`flow-vendor`)

- `crates/<crate>/`: materialized crate source trees.
- `manifests/<crate>.toml`: per-crate sync metadata.
- `profiles/flow.toml`: crate list for Flow hydration.

## Flow Workflow

1. Initialize vendor checkout (local clone or local bootstrap):

```bash
scripts/vendor/vendor-repo.sh init
```

2. Import current local materialized crates into vendor repo and pin lock commit:

```bash
scripts/vendor/vendor-repo.sh import-local
```

3. Hydrate `lib/vendor/*` from pinned vendor commit:

```bash
scripts/vendor/vendor-repo.sh hydrate
# equivalent default path:
scripts/vendor/materialize-all.sh
```

4. Inspect lock/checkout/remote state:

```bash
scripts/vendor/vendor-repo.sh status
```

5. Push vendor repo updates:

```bash
scripts/vendor/vendor-repo.sh push
```

6. Re-pin lock to a specific vendor commit if needed:

```bash
scripts/vendor/vendor-repo.sh pin <commit>
```

## Upstream Sync Loop

Keep existing update policy scripts:

```bash
scripts/vendor/check-upstream.sh --important
scripts/vendor/sync-all.sh --important --dry-run
scripts/vendor/sync-all.sh --important
```

After syncing local vendored crates, import + pin to vendor repo:

```bash
scripts/vendor/vendor-repo.sh import-local
```

## Fallback Mode (Cargo Cache)

If you intentionally want the old local-cache materialization path:

```bash
scripts/vendor/materialize-all.sh --from-cache
```

## Guardrails

- Commit only lock/metadata/scripts/docs in `flow`; keep vendor source in `flow-vendor`.
- Keep trim logic deterministic in `scripts/vendor/apply-trims.sh`.
- Keep patch updates default; minor/major only in explicit review windows.
- Always run `cargo check` after hydration/sync.
