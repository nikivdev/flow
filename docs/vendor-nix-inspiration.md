# Nix Ideas In Cargo-First Vendoring

This vendoring model does not replace Cargo with Nix, but it borrows core Nix ideas
to get reproducibility, control, and fast iteration.

## What We Borrow

### 1. Pinned, immutable inputs

- `vendor.lock.toml` pins vendored source by exact commit.
- `Cargo.lock` pins resolved crate versions.
- CI/local hydrate from pinned state, not floating latest.

Nix analogy: flake/lock pinning exact inputs.

### 2. Content/provenance tracking

- `lib/vendor-manifest/<crate>.toml` records checksums and upstream metadata.
- `verify --strict-provenance` enforces provenance completeness.

Nix analogy: content-addressed trust and auditable input provenance.

### 3. Declarative materialization

- `scripts/vendor/vendor-repo.sh hydrate` materializes `lib/vendor/*` from lock state.
- Build uses explicit path patches in `Cargo.toml`.

Nix analogy: declarative store realization from a locked graph.

### 4. Transactional updates and rollback safety

- `vendor-control.sh inhouse` snapshots and rolls back on failure.
- Vendor pin can be moved back to a known good commit.

Nix analogy: atomic generation switch + rollback.

### 5. Source ownership as a separate store

- source churn lives in `flow-vendor`,
- product wiring and pins live in `flow`.

Nix analogy: separate immutable store objects from top-level project logic.

### 6. Minimize closure size

- remove unused features/deps/macros from vendored crates,
- track duplicate versions and offender crates (`cargo tree -d`, `offenders.sh`).

Nix analogy: reducing closure size to speed builds and improve iteration.

## Why This Matters For Our Goals

- Faster compile/iteration: smaller dependency surface, less macro/dependency overhead.
- Full control: direct edits in vendored crates when needed.
- Reliable upstream sync: scripted update loop with lock pinning and provenance checks.
- Reproducible builds: same vendored commit + same lockfile => same source graph.

## Practical Loop

```bash
/Users/nikiv/code/rise/scripts/vendor-control.sh sync --project /Users/nikiv/code/flow -- --important --dry-run
/Users/nikiv/code/rise/scripts/vendor-control.sh sync --project /Users/nikiv/code/flow -- --important
/Users/nikiv/code/rise/scripts/vendor-control.sh verify --project /Users/nikiv/code/flow --strict-provenance
scripts/vendor/vendor-repo.sh hydrate
cargo check -q
```

This gives a Nix-like operational discipline while preserving Cargo ecosystem behavior.
