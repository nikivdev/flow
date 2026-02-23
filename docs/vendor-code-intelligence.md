# Vendor Code Intelligence (Typesense)

This document defines the crates-focused equivalent of `opensrc` for Flow vendoring.

## Goal

Keep Cargo as resolver/build authority while adding very fast local search across:

- first-party code (`src`, `crates`, `scripts`, `docs`, `tests`),
- vendored crates (`lib/vendor/*`),
- source metadata (`lib/vendor-manifest/*.toml` + `vendor.lock.toml`).

This gives AI and humans a fast map of "what code do we own right now?" without remote lookups.

## Why This Exists

Vendoring gives full control, but it increases local code volume.
Without a fast index, trim/rewrite/sync loops become slower.

Typesense indexing solves that by keeping an always-queryable local code/search layer.

## Commands

Start local Typesense (shared launcher in `~/code/infra/base`):

```bash
f vendor-typesense-setup   # one-time install in flox if needed
f vendor-typesense-up
f vendor-typesense-status
```

Build index:

```bash
f vendor-code-index
```

Search code chunks:

```bash
f vendor-code-search "Router"
f vendor-code-search "serde_json" --scope vendor --crate reqwest
f vendor-code-search "spawn" --scope firstparty --lang rs
```

Search source inventory:

```bash
f vendor-code-search-sources "ratatui"
f vendor-code-search-sources "github.com" --limit 50
```

Inspect raw inventory:

```bash
f vendor-code-sources
```

## Data Model

The index script (`scripts/vendor/typesense_code_index.py`) writes:

- `.vendor/typesense/sources.json`:
  - opensrc-style local source inventory for vendored + first-party scopes.
- Typesense `<prefix>_sources` collection:
  - per source (crate/repo) metadata: name, version, materialized path, upstream, checksum.
- Typesense `<prefix>_chunks` collection:
  - chunked code text + path + scope + crate + symbols + line ranges.

Default prefix is `flow_code`, so the default collections are:

- `flow_code_sources`
- `flow_code_chunks`

## How It Stays Aligned With Upstream

`vendor.lock.toml` is the canonical vendored crate set.
`lib/vendor-manifest/*.toml` is the per-crate provenance and sync state.

The index script reads both, so every `inhouse/sync/hydrate` cycle can be followed by:

```bash
f vendor-code-index
```

This keeps search aligned with the exact pinned state currently compiled by Cargo.

## Operational Loop

1. Sync or inhouse crates (`vendor-control.sh`, `scripts/vendor/sync-*`).
2. Re-index (`f vendor-code-index`).
3. Search for dead APIs/deps/macros and trim targets (`f vendor-code-search ...`).
4. Validate (`cargo check`, vendoring verify gates).
5. Commit source churn in `flow-vendor`, pin updates in `flow`.

## Notes

- The indexer is local-first and does not replace Cargo metadata.
- Use `--dry-run` for large experiments before writing collections.
- Use `--max-files` for quick smoke indexing in CI/debug runs.
