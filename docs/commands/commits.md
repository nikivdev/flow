# f commits

Browse commits with AI metadata and mark notable commits for quick access.

## Usage

```bash
f commits
f commits --limit 200
f commits --all
f commits top
f commits mark <hash>
f commits unmark <hash>
```

## Notable commits

Notable commits are stored in `.ai/internal/commits/top.txt` in the repo root. Each line is:

```
<full-hash>\t<label>
```

## Key bindings

When using fzf:

- `ctrl-t` — toggle notable for the selected commit.
