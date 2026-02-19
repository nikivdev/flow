# Commit Explanations (Generated)

`f explain-commits` writes generated artifacts into this folder by default:

- `*.md`: human-readable commit explanations
- `*.json`: machine-readable sidecars
- `.index.json`: digest/index cache

Policy:

- This directory is treated as local generated output by default.
- Generated files are gitignored to keep normal commits clean.
- If you want tracked artifacts, set `[explain-commits].output_dir` in `flow.toml` to a different directory and commit that directory intentionally.
