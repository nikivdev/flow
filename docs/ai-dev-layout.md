# `.ai/` Dev Layout

Use `.ai/` aggressively for local AI-assisted development, but keep the split
between tracked project intelligence and disposable local artifacts explicit.

Tracked in this repo:

- `.ai/docs/`
- `.ai/recipes/`
- `.ai/repos.toml`
- curated `.ai/skills/` entries that are intentionally committed

Ignored local-dev buckets:

- `.ai/reviews/`
  - generated PR feedback packets and review snapshots
- `.ai/test/`
  - AI scratch tests and local validation files
- `.ai/tmp/`
  - throwaway intermediate files
- `.ai/cache/`
  - cached derivations or precomputed local state
- `.ai/artifacts/`
  - generated outputs worth inspecting locally but not committing
- `.ai/traces/`
  - local trace dumps or trace export scratch
- `.ai/generated/`
  - one-off generated code/docs that should be promoted elsewhere if kept
- `.ai/scratch/`
  - free-form local notes, prompts, and experiments

Rules of thumb:

1. If it is canonical project knowledge, promote it out of the local bucket and
   track it intentionally.
2. If it is generated, per-machine, or iteration-only, keep it under one of the
   ignored buckets above.
3. Prefer `.ai/` over random top-level temp files so local AI/dev state stays
   contained and easy to clean up.
