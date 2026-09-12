#!/usr/bin/env bash
# D0-5 dogfood: a real Claude Code coordinator drives Remuda over MCP.
#
# Starts remuda dev (loopback Hub + native Node), then:
#   claude -p --model haiku --max-budget-usd 0.5 \
#     --mcp-config docs/design/remuda-mcp.json
# The prompt tells haiku to create a worktree, start codex (pty) and grok (pty)
# instances with task briefs, wait until each prints DONE, read, and stop.
# Stream-json is redacted into docs/design/evidence/dogfood-1.jsonl.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
EVIDENCE="${REMUDA_DOGFOOD_EVIDENCE:-$ROOT/docs/design/evidence/dogfood-1.jsonl}"
MCP_CONFIG="$ROOT/docs/design/remuda-mcp.json"
DEMO_DIR="${REMUDA_DOGFOOD_DIR:-${TMPDIR:-/tmp}/remuda-dogfood-1}"
# Override when 18080/18787 are already bound (coordinator demo).
HUB_LISTEN="${REMUDA_DOGFOOD_HUB_LISTEN:-127.0.0.1:18080}"
NODE_LISTEN="${REMUDA_DOGFOOD_NODE_LISTEN:-127.0.0.1:18787}"
ACCESS_CODE="${REMUDA_DOGFOOD_ACCESS_CODE:-dogfood-1-access}"
WORKTREE_NAME="${REMUDA_DOGFOOD_WORKTREE:-df1}"
# Sibling of this worktree (`../df1` from remuda-wt/x-proto2), not the
# `../remuda-wt/<name>` default which would nest remuda-wt/remuda-wt.
WORKTREE_PATH="${REMUDA_DOGFOOD_WORKTREE_PATH:-$(cd "$ROOT/.." && pwd)/${WORKTREE_NAME}}"

REMUDA_PID=""
REMUDA_BIN=""
TOKEN=""
HUB_URL=""

log() { printf '%s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

target_dir() {
  local dir="${CARGO_TARGET_DIR:-${CARGO_BUILD_TARGET_DIR:-}}"
  if [[ -n "$dir" ]]; then
    if [[ "$dir" = /* ]]; then
      printf '%s\n' "$dir"
    else
      printf '%s\n' "$ROOT/$dir"
    fi
  else
    printf '%s\n' "$ROOT/target"
  fi
}

usage() {
  cat <<'EOF' >&2
Usage: scripts/demo/dogfood-1.sh [--skip-live]

  --skip-live   start remuda dev and write mcp-config, but do not call claude
EOF
}

SKIP_LIVE=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-live) SKIP_LIVE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage; exit 2 ;;
  esac
done

cleanup() {
  if [[ -n "$REMUDA_PID" ]] && kill -0 "$REMUDA_PID" 2>/dev/null; then
    kill "$REMUDA_PID" 2>/dev/null || true
    wait "$REMUDA_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

remove_dogfood_worktree() {
  local repo_root
  repo_root="$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
  local wt_path="$WORKTREE_PATH"
  if [[ -d "$wt_path" ]]; then
    git -C "$ROOT" worktree remove --force "$wt_path" >/dev/null 2>&1 || true
  fi
  git -C "$ROOT" branch -D "wt/${WORKTREE_NAME}/work" >/dev/null 2>&1 || true
  git -C "$ROOT" branch -D "wt/${WORKTREE_NAME}/work-2" >/dev/null 2>&1 || true
  if [[ -n "$repo_root" && -f "$repo_root/remuda-worktrees.json" ]]; then
    python3 - "$repo_root/remuda-worktrees.json" "$WORKTREE_NAME" <<'PY' || true
import json, sys
path, name = sys.argv[1], sys.argv[2]
try:
    data = json.load(open(path))
except Exception:
    raise SystemExit(0)
rows = [row for row in data.get("worktrees", []) if row.get("name") != name]
if len(rows) != len(data.get("worktrees", [])):
    data["worktrees"] = rows
    with open(path, "w") as fh:
        json.dump(data, fh, indent=2)
        fh.write("\n")
PY
  fi
}

redact_stream() {
  python3 - "$1" "$2" "$ACCESS_CODE" "$HUB_URL" "$DEMO_DIR" "$HOME" "$TOKEN" <<'PY'
import json, os, re, sys
src, dest, bootstrap, hub, workdir, home, token = sys.argv[1:8]
secrets = [s for s in (bootstrap, token, os.environ.get("ANTHROPIC_API_KEY", ""), os.environ.get("REMUDA_TOKEN", ""), os.environ.get("REMUDA_BOOTSTRAP_TOKEN", "")) if s]
secrets.sort(key=len, reverse=True)

def scrub(obj):
    if isinstance(obj, str):
        out = obj
        for s in secrets:
            if s:
                out = out.replace(s, "<redacted>")
        if home:
            out = out.replace(home, "<home>")
        out = re.sub(r"/Users/[A-Za-z0-9._-]*", "<home>", out)
        if workdir:
            out = out.replace(workdir, "<workdir>")
        if hub:
            out = out.replace(hub, "http://127.0.0.1:<hub>")
        out = re.sub(r"sk-[A-Za-z0-9_-]{8,}", "sk-<redacted>", out)
        out = re.sub(r"agk_[A-Za-z0-9]+", "agk_<redacted>", out)
        out = re.sub(r"Bearer [A-Za-z0-9._-]+", "Bearer <redacted>", out)
        out = re.sub(r"boot-[a-f0-9]+", "boot-<redacted>", out)
        out = re.sub(r"hst_[A-Za-z0-9-]+", "hst_<redacted>", out)
        return out
    if isinstance(obj, list):
        return [scrub(x) for x in obj]
    if isinstance(obj, dict):
        redacted = {}
        for k, v in obj.items():
            kl = k.lower()
            if kl in ("token", "bootstraptoken", "authorization", "api_key", "apikey", "secret"):
                redacted[k] = "<redacted>"
            else:
                redacted[k] = scrub(v)
        return redacted
    return obj

lines = []
meta = {
    "_meta": {
        "source": "scripts/demo/dogfood-1.sh",
        "redacted": True,
        "model": "haiku",
        "maxBudgetUsd": 0.5,
        "hub": hub,
    }
}
round_name = os.environ.get("REMUDA_DOGFOOD_ROUND")
if round_name:
    meta["_meta"]["round"] = round_name
lines.append(json.dumps(meta, sort_keys=True))
if os.path.isfile(src):
    with open(src, encoding="utf-8", errors="replace") as fh:
        for raw in fh:
            raw = raw.strip()
            if not raw:
                continue
            try:
                obj = json.loads(raw)
            except json.JSONDecodeError:
                lines.append(json.dumps({"type": "raw", "text": scrub(raw)}))
                continue
            lines.append(json.dumps(scrub(obj), sort_keys=True))
os.makedirs(os.path.dirname(dest), exist_ok=True)
with open(dest, "w", encoding="utf-8") as out:
    out.write("\n".join(lines) + "\n")
print(f"wrote {dest} ({len(lines)} lines)", file=sys.stderr)
PY
}

http_json() {
  local method="$1" url="$2" body="${3:-}"
  python3 - "$method" "$url" "$body" "${TOKEN:-}" "${ACCESS_CODE}" <<'PY'
import json, sys, urllib.error, urllib.request
method, url, body, token, access = sys.argv[1:6]
headers = {"Accept": "application/json"}
if body:
    headers["Content-Type"] = "application/json"
if token:
    headers["Authorization"] = "Bearer " + token
if access:
    headers["X-Remuda-Access-Code"] = access
data = body.encode() if body else None
req = urllib.request.Request(url, data=data, method=method, headers=headers)
try:
    with urllib.request.urlopen(req, timeout=8) as resp:
        raw = resp.read().decode()
        print(json.dumps({"status": resp.status, "body": json.loads(raw) if raw.strip() else None}))
except urllib.error.HTTPError as err:
    raw = err.read().decode(errors="replace")
    try:
        parsed = json.loads(raw) if raw.strip() else raw
    except json.JSONDecodeError:
        parsed = raw
    print(json.dumps({"status": err.code, "body": parsed}))
except Exception as err:
    print(json.dumps({"status": 0, "body": str(err)}))
    sys.exit(2)
PY
}

wait_tcp() {
  local addr="$1"
  python3 - "$addr" <<'PY'
import socket, sys
host, port = sys.argv[1].rsplit(":", 1)
s = socket.socket()
s.settimeout(0.2)
try:
    s.connect((host, int(port)))
except OSError:
    sys.exit(1)
finally:
    s.close()
PY
}

step_build() {
  log "==> build remuda"
  mkdir -p "$DEMO_DIR/logs"
  local logf="$DEMO_DIR/logs/build.log"
  local target
  target="$(target_dir)"
  export CARGO_TARGET_DIR="$target"
  if ! (cd "$ROOT" && cargo build -p remuda --locked) >"$logf" 2>&1; then
    die "cargo build -p remuda --locked failed"$'\n'"$(tail -n 40 "$logf")"
  fi
  REMUDA_BIN="$target/debug/remuda"
  [[ -x "$REMUDA_BIN" ]] || die "missing $REMUDA_BIN"
  export PATH="$(dirname "$REMUDA_BIN"):$PATH"
}

step_dev() {
  log "==> remuda dev hub=$HUB_LISTEN node=$NODE_LISTEN"
  mkdir -p "$DEMO_DIR/data"
  umask 077
  printf '%s\n' "$ACCESS_CODE" >"$DEMO_DIR/access-code"
  chmod 600 "$DEMO_DIR/access-code"
  export REMUDA_DATA_DIR="$DEMO_DIR/data"
  export REMUDA_COOKIE_SECURE=0
  local logf="$DEMO_DIR/logs/dev.log"
  local cmd=(
    "$REMUDA_BIN" --data-dir "$DEMO_DIR/data"
    dev
    --listen "$NODE_LISTEN"
    --hub-listen "$HUB_LISTEN"
    --access-code-file "$DEMO_DIR/access-code"
    --workspace "$ROOT"
  )
  "${cmd[@]}" >"$logf" 2>&1 &
  REMUDA_PID=$!
  local i
  for i in $(seq 1 80); do
    if ! kill -0 "$REMUDA_PID" 2>/dev/null; then
      die "remuda dev exited"$'\n'"$(tail -n 40 "$logf")"
    fi
    if wait_tcp "$HUB_LISTEN"; then
      log "remuda dev listening pid=$REMUDA_PID"
      return 0
    fi
    sleep 0.2
  done
  die "timed out waiting for $HUB_LISTEN"$'\n'"$(tail -n 40 "$logf")"
}

write_runtime_mcp_config() {
  local dest="$DEMO_DIR/mcp-config.json"
  python3 - "$MCP_CONFIG" "$dest" "$REMUDA_BIN" "$HUB_URL" "$TOKEN" "$ACCESS_CODE" <<'PY'
import json, sys
src, dest, command, hub, token, bootstrap = sys.argv[1:7]
with open(src, encoding="utf-8") as fh:
    cfg = json.load(fh)
server = cfg.setdefault("mcpServers", {}).setdefault("remuda", {})
server["command"] = command
server["args"] = ["mcp"]
env = server.setdefault("env", {})
env["REMUDA_HUB"] = hub
env["REMUDA_TOKEN"] = token
env["REMUDA_BOOTSTRAP_TOKEN"] = bootstrap
env["RUST_LOG"] = "warn"
with open(dest, "w", encoding="utf-8") as fh:
    json.dump(cfg, fh, indent=2)
    fh.write("\n")
print(dest)
PY
}

step_login() {
  log "==> login Hub"
  HUB_URL="http://$HUB_LISTEN"
  local resp
  resp="$(http_json POST "$HUB_URL/v1/login" "{\"bootstrapToken\":\"$ACCESS_CODE\",\"deviceName\":\"dogfood-1\"}")" \
    || die "login failed: $resp"
  local status
  status="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])' <<<"$resp")"
  TOKEN="$(python3 -c 'import json,sys; b=json.load(sys.stdin)["body"] or {}; print(b.get("token") or "")' <<<"$resp")"
  if [[ "$status" != "200" || -z "$TOKEN" ]]; then
    die "login HTTP $status $resp"
  fi
  export TOKEN
  export REMUDA_HUB="$HUB_URL"
  export REMUDA_TOKEN="$TOKEN"
  export REMUDA_BOOTSTRAP_TOKEN="$ACCESS_CODE"
  local i hosts
  for i in $(seq 1 60); do
    hosts="$(http_json GET "$HUB_URL/v1/hosts" || true)"
    if python3 -c 'import json,sys; d=json.load(sys.stdin); items=(d.get("body") or {}).get("items") or []; sys.exit(0 if items else 1)' <<<"$hosts"; then
      log "host enrolled"
      return 0
    fi
    sleep 0.25
  done
  die "timed out waiting for Hub host enrollment"$'\n'"$hosts"
}

run_claude() {
  local attempt="$1"
  local raw="$DEMO_DIR/claude-$attempt.jsonl"
  local prompt
  prompt="$(cat <<PROMPT
You are the Remuda dogfood coordinator. Use only Remuda MCP tools (names remuda_worktree_create, remuda_instance_create, remuda_instance_wait, remuda_instance_send, remuda_instance_read, remuda_instance_stop, remuda_instance_list). Do not use Bash. Do not compile. Do not mention tokens.

Work from this git repository: ${ROOT}
Use worktree name "${WORKTREE_NAME}" and path "${WORKTREE_PATH}". If remuda_worktree_create needs a repo path, pass repo="${ROOT}".

Do these steps in order:

1. Call remuda_worktree_create with name="${WORKTREE_NAME}", base="main", repo="${ROOT}", path="${WORKTREE_PATH}".
2. Start two PTY instances in that worktree, in parallel if the tools allow, otherwise one after the other:
   - remuda_instance_create kind="codex" driver="pty" worktree="${WORKTREE_NAME}" name="df-codex" prompt="In the current directory create a file named dogfood-codex.txt containing exactly the line codex-ok. Then print a line that starts with DONE (for example: DONE). Do not wait for further input."
   - remuda_instance_create kind="grok" driver="pty" worktree="${WORKTREE_NAME}" name="df-grok" prompt="In the current directory create a file named dogfood-grok.txt containing exactly the line grok-ok. Then print a line that starts with DONE (for example: DONE). Do not wait for further input."
3. From each create result, parse instanceId (field instance.instanceId or instanceId).
4. For each instance, call remuda_instance_wait with until="line:(?m)^DONE" and timeoutMs=180000. Do not treat the task brief itself as a match. Done is true only when the wait JSON reason is "condition-met" (matchedLine may be "• DONE" or similar). If reason is timeout, call remuda_instance_send with text "Print DONE as its own line now." then wait once more with timeoutMs=60000.
5. For each instance, call remuda_instance_read with source="screen" and lines=80. If screen is empty, retry with source="journal".
6. For each instance, call remuda_instance_stop with scope="instance".
7. Reply with one JSON object only:
{"codexId":"...","grokId":"...","codexDone":true,"grokDone":true,"notes":"..."}
PROMPT
)"
  local -a cmd=(
    claude -p "$prompt"
    --model haiku
    --max-budget-usd 0.5
    --output-format stream-json
    --verbose
    --mcp-config "$DEMO_DIR/mcp-config.json"
    --strict-mcp-config
    --permission-mode dontAsk
    --no-session-persistence
  )
  if [[ "$attempt" == "1" ]]; then
    cmd+=(
      --allowedTools
      mcp__remuda__remuda_worktree_create,mcp__remuda__remuda_instance_create,mcp__remuda__remuda_instance_wait,mcp__remuda__remuda_instance_send,mcp__remuda__remuda_instance_read,mcp__remuda__remuda_instance_stop,mcp__remuda__remuda_instance_list
    )
  fi
  log "==> claude attempt $attempt"
  mkdir -p "$DEMO_DIR/claude-config"
  chmod 0700 "$DEMO_DIR/claude-config"
  local status=0
  (
    cd "$ROOT"
    env -u CLAUDE_CODE_SIMPLE \
      "${cmd[@]}"
  ) >"$raw" 2>"$DEMO_DIR/claude-$attempt.err" || status=$?
  log "claude attempt $attempt exit $status"
  printf '%s\n' "$status"
}

called_remuda() {
  python3 - "$1" <<'PY'
import json, sys
path = sys.argv[1]
ok = False
try:
    for line in open(path, encoding="utf-8", errors="replace"):
        line = line.strip()
        if not line:
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        if obj.get("type") != "assistant":
            continue
        for c in (obj.get("message") or {}).get("content") or []:
            if isinstance(c, dict) and c.get("type") == "tool_use" and "remuda_" in str(c.get("name", "")):
                ok = True
except FileNotFoundError:
    pass
sys.exit(0 if ok else 1)
PY
}

main() {
  command -v cargo >/dev/null || die "cargo is required"
  command -v python3 >/dev/null || die "python3 is required"
  command -v claude >/dev/null || die "claude is required on PATH"
  command -v grok >/dev/null || die "grok is required on PATH"
  command -v codex >/dev/null || die "codex is required on PATH"
  command -v herdr >/dev/null || die "herdr is required on PATH"
  [[ -f "$MCP_CONFIG" ]] || die "missing $MCP_CONFIG"
  mkdir -p "$DEMO_DIR"
  chmod 0700 "$DEMO_DIR"
  mkdir -p "$DEMO_DIR/logs" "$ROOT/docs/design/evidence"
  remove_dogfood_worktree
  step_build
  step_dev
  step_login
  write_runtime_mcp_config
  log "mcp-config $DEMO_DIR/mcp-config.json (from $MCP_CONFIG)"
  if [[ "$SKIP_LIVE" -eq 1 ]]; then
    log "skip live claude; remuda dev still running until exit"
    return 0
  fi
  local st1 st2
  st1="$(run_claude 1)"
  if called_remuda "$DEMO_DIR/claude-1.jsonl"; then
    redact_stream "$DEMO_DIR/claude-1.jsonl" "$EVIDENCE"
  else
    log "attempt 1 did not call remuda_*; retrying without --allowedTools"
    st2="$(run_claude 2)"
    redact_stream "$DEMO_DIR/claude-2.jsonl" "$EVIDENCE"
    if [[ "${st2:-}" != "0" && "$st1" != "0" ]]; then
      log "warning: both claude attempts exited non-zero (1=$st1 2=$st2); evidence still written"
    fi
  fi
  log "evidence $EVIDENCE"
}

main "$@"
