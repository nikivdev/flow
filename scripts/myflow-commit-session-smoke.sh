#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: myflow-commit-session-smoke.sh [options]

Verify that a commit is visible in myflow and optionally require attached AI sessions.

Options:
  --repo-path PATH         Git repo to inspect (default: current directory)
  --repo-slug OWNER/REPO   Override repo slug (auto-detected from origin)
  --commit-sha SHA         Commit to verify (default: HEAD of --repo-path)
  --api-base URL           myflow API base (default: MYFLOW_URL or https://myflow.sh)
  --token TOKEN            Auth token (default: MYFLOW_TOKEN or ~/.config/flow/auth.toml)
  --timeout SECONDS        Poll timeout waiting for commit (default: 60)
  --require-sessions       Fail if commit has zero attached sessions
  --skip-session-fetch     Do not verify GET /api/sessions/:id for first session
  -h, --help               Show this help
EOF
}

REPO_PATH="${PWD}"
REPO_SLUG=""
COMMIT_SHA=""
API_BASE="${MYFLOW_URL:-https://myflow.sh}"
TOKEN="${MYFLOW_TOKEN:-}"
TIMEOUT_SECS=60
REQUIRE_SESSIONS=0
SKIP_SESSION_FETCH=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --repo-path)
      REPO_PATH="$2"
      shift 2
      ;;
    --repo-slug)
      REPO_SLUG="$2"
      shift 2
      ;;
    --commit-sha)
      COMMIT_SHA="$2"
      shift 2
      ;;
    --api-base)
      API_BASE="$2"
      shift 2
      ;;
    --token)
      TOKEN="$2"
      shift 2
      ;;
    --timeout)
      TIMEOUT_SECS="$2"
      shift 2
      ;;
    --require-sessions)
      REQUIRE_SESSIONS=1
      shift
      ;;
    --skip-session-fetch)
      SKIP_SESSION_FETCH=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [ ! -d "$REPO_PATH/.git" ]; then
  echo "repo path is not a git repo: $REPO_PATH" >&2
  exit 2
fi

if [ -z "$COMMIT_SHA" ]; then
  COMMIT_SHA="$(git -C "$REPO_PATH" rev-parse HEAD)"
fi

if [ -z "$REPO_SLUG" ]; then
  origin="$(git -C "$REPO_PATH" remote get-url origin 2>/dev/null || true)"
  if [ -n "$origin" ]; then
    if [[ "$origin" =~ ^git@github\.com:(.+)\.git$ ]]; then
      REPO_SLUG="${BASH_REMATCH[1]}"
    elif [[ "$origin" =~ ^git@github\.com:(.+)$ ]]; then
      REPO_SLUG="${BASH_REMATCH[1]}"
    elif [[ "$origin" =~ ^https?://github\.com/(.+)\.git$ ]]; then
      REPO_SLUG="${BASH_REMATCH[1]}"
    elif [[ "$origin" =~ ^https?://github\.com/(.+)$ ]]; then
      REPO_SLUG="${BASH_REMATCH[1]}"
    fi
  fi
fi

if [ -z "$REPO_SLUG" ]; then
  echo "failed to resolve repo slug; pass --repo-slug owner/repo" >&2
  exit 2
fi

if [ -z "$TOKEN" ] && [ -f "$HOME/.config/flow/auth.toml" ]; then
  TOKEN="$(
    python3 - "$HOME/.config/flow/auth.toml" <<'PY'
import pathlib
import sys

p = pathlib.Path(sys.argv[1])
if not p.exists():
    print("")
    raise SystemExit(0)

try:
    import tomllib
except Exception:
    print("")
    raise SystemExit(0)

try:
    data = tomllib.loads(p.read_text(encoding="utf-8"))
except Exception:
    print("")
    raise SystemExit(0)

token = data.get("token")
print(token if isinstance(token, str) else "")
PY
  )"
fi

if [ -z "$TOKEN" ]; then
  echo "missing token; set MYFLOW_TOKEN or pass --token" >&2
  exit 2
fi

API_BASE="${API_BASE%/}"
ENCODED_REPO="$(
  python3 - "$REPO_SLUG" <<'PY'
import sys, urllib.parse
print(urllib.parse.quote(sys.argv[1], safe=""))
PY
)"

deadline=$(( $(date +%s) + TIMEOUT_SECS ))
commit_line=""
payload=""

echo "[myflow-smoke] repo=${REPO_SLUG} commit=${COMMIT_SHA} api=${API_BASE}"

while [ "$(date +%s)" -le "$deadline" ]; do
  payload="$(
    curl -fsS \
      --max-time 10 \
      -H "Authorization: Bearer ${TOKEN}" \
      "${API_BASE}/api/commits?repo=${ENCODED_REPO}" \
      || true
  )"

  if [ -n "$payload" ]; then
    set +e
    commit_line="$(
      python3 - "$COMMIT_SHA" "$REQUIRE_SESSIONS" <<'PY' <<<"$payload"
import json
import sys

target = sys.argv[1].lower()
require_sessions = sys.argv[2] == "1"

try:
    data = json.load(sys.stdin)
except Exception:
    raise SystemExit(5)

if not isinstance(data, list):
    raise SystemExit(5)

found = None
for item in data:
    if not isinstance(item, dict):
        continue
    sha = str(item.get("commitSha", "")).lower()
    if sha == target or sha.startswith(target):
        found = item
        break

if not found:
    raise SystemExit(3)

sessions = found.get("sessions") or []
if not isinstance(sessions, list):
    sessions = []

if require_sessions and len(sessions) == 0:
    raise SystemExit(4)

window = found.get("sessionWindow") or {}
mode = ""
if isinstance(window, dict):
    mode = str(window.get("mode", ""))

first_session = ""
if sessions and isinstance(sessions[0], dict):
    first_session = str(sessions[0].get("sessionId", ""))

print(f"{found.get('commitSha','')}\t{len(sessions)}\t{mode}\t{first_session}")
PY
    )"
    status=$?
    set -e

    case "$status" in
      0) break ;;
      3) ;;
      4)
        echo "[myflow-smoke] commit found but sessions=0 and --require-sessions is set" >&2
        exit 1
        ;;
      *)
        ;;
    esac
  fi

  sleep 2
done

if [ -z "$commit_line" ]; then
  echo "[myflow-smoke] commit not found in myflow within ${TIMEOUT_SECS}s" >&2
  exit 1
fi

IFS=$'\t' read -r found_sha session_count window_mode first_session_id <<<"$commit_line"
echo "[myflow-smoke] found commit=${found_sha} sessions=${session_count} sessionWindow.mode=${window_mode:-<none>}"

if [ "$SKIP_SESSION_FETCH" -eq 0 ] && [ -n "$first_session_id" ]; then
  encoded_session="$(
    python3 - "$first_session_id" <<'PY'
import sys, urllib.parse
print(urllib.parse.quote(sys.argv[1], safe=""))
PY
  )"
  curl -fsS \
    --max-time 10 \
    -H "Authorization: Bearer ${TOKEN}" \
    "${API_BASE}/api/sessions/${encoded_session}" >/dev/null
  echo "[myflow-smoke] verified first session fetch: ${first_session_id}"
fi

echo "[myflow-smoke] ok"
