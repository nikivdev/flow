# ASCII Commit Visualization Pipeline

This explains how commit analysis data becomes ASCII-style diagrams in myflow.

Scope:

- generation and storage in Flow
- API serving via Flow server
- runtime diagram rendering in myflow via `box-of-rain`

---

## 1. Commit Analysis Generation

Flow generates commit explanations with:

```bash
f explain-commits 3 --force
```

Implementation:

- `src/explain_commits.rs`
- Uses `ai-task.sh` with provider/model fixed to Kimi defaults (`nvidia` + `moonshotai/kimi-k2.5`).
- For each commit, Flow gathers:
  - `sha`, `short_sha`, `subject`, `author`, `date`
  - file list from `git diff --name-only`
  - truncated diff payload (max chars guard)

Output per project (default):

- `docs/commits/<date>-<short_sha>-<slug>.md`
- `docs/commits/<date>-<short_sha>-<slug>.json`
- `docs/commits/.index.json` (digest/index cache)

Notes:

- `--force` bypasses digest skip logic.
- `--out-dir` can override default output location.

---

## 2. Storage Format

The sidecar `.json` mirrors Flow’s `ExplainedCommit` shape:

- `sha`
- `short_sha`
- `subject`
- `author`
- `date`
- `summary`
- `changes`
- `files` (array of changed file paths)
- `markdown_file`
- `generated_at`

This is the source of truth consumed by the UI.

---

## 3. API Serving Layer

Flow server exposes commit explanations over HTTP:

- `GET /projects/:name/commit-explanations?limit=50`
- `GET /projects/:name/commit-explanations/:sha`

Implementation:

- `src/log_server.rs`:
  - `project_commit_explanations`
  - `project_commit_explanation_detail`
- data loader functions are in `src/explain_commits.rs`:
  - `list_explained_commits`
  - `get_explained_commit`

---

## 4. myflow Data Consumption

myflow fetches these endpoints through `flowFetch` model atoms:

- `/projects/$project/commit-explanations`
- `/projects/$project/commit-explanations/$sha`

Model file:

- `~/code/myflow/web/lib/models/flow-projects.ts`

Relevant type:

- `FlowExplainedCommit`

---

## 5. Diagram Generation (ASCII -> SVG)

Diagram rendering is client-side in myflow and uses `box-of-rain`.

Theme/options:

- `~/code/myflow/web/lib/diagram-theme.ts`
- shared `DIAGRAM_SVG_OPTIONS`:
  - transparent background
  - mono font
  - light/dark foreground colors

### Timeline diagram

File:

- `~/code/myflow/web/lib/commit-timeline-diagram.tsx`

Algorithm:

1. take up to 8 newest commits
2. reverse to oldest -> newest
3. build one rounded node per commit:
   - line 1: `short_sha`
   - lines 2+: truncated subject (2 lines max)
4. connect node `i -> i+1` (right to left sides)
5. render:
   - `render(nodeDef)` -> ASCII layout
   - `renderSvg(ascii, DIAGRAM_SVG_OPTIONS)` -> SVG
6. inject SVG into DOM with `dangerouslySetInnerHTML`

Mounted at:

- `~/code/myflow/web/pages/flow/$project/index.tsx`

### Files impact diagram

File:

- `~/code/myflow/web/lib/files-impact-diagram.tsx`

Algorithm:

1. group `commit.files` by top path bucket:
   - first 2 segments when possible
2. create bold commit node
3. create one rounded directory node per group:
   - dir label
   - up to 3 file names
   - `+N more` overflow line
4. connect `commit -> each_dir`
5. render ASCII then SVG with same theme options

Mounted at:

- `~/code/myflow/web/pages/flow/$project/commit/$sha.tsx`

---

## 6. Performance and Limits

- both diagram components are wrapped in `useMemo`
- timeline hard limit: 8 commits
- subject lines are truncated for stable node widths
- files list per group is capped in-node (full list still shown below diagram)

---

## 7. Common Failure Modes

1. No commit data:
   - run `f explain-commits N` in the target repo
2. API empty:
   - ensure Flow server is running (`f server`)
   - ensure project is registered in Flow
3. Diagram package missing:
   - ensure `box-of-rain` dependency resolves in myflow runtime build
4. BetterAuth `/api` base URL error in browser:
   - use absolute URL normalization in `web/lib/auth-client.ts` (relative `/api` alone is invalid for BetterAuth client config)

---

## 8. Quick End-to-End Check

From target repo (example: codex):

```bash
cd ~/repos/openai/codex
f explain-commits 3 --force
```

From Flow:

```bash
f server --host 127.0.0.1 --port 9050
curl 'http://127.0.0.1:9050/projects/codex/commit-explanations?limit=3'
```

Then open myflow:

- project view: `/flow/codex`
- commit view: `/flow/codex/commit/<sha>`
