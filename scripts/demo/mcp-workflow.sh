#!/usr/bin/env bash
# End-to-end: main Claude agent drives Remuda workers over MCP.
#
# Starts an in-process Hub with a fake Node (labels region=sg), then runs
# `claude -p --model haiku` with --mcp-config pointing at `remuda mcp`.
# Stream-json is redacted into docs/design/evidence/mcp-workflow.jsonl.
#
# Live model: at most two haiku runs, --max-budget-usd 0.3, isolated cwd
# /tmp/remuda-mcp-workflow (0700). Does not use the operator's Claude home.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
EVIDENCE="$ROOT/docs/design/evidence/mcp-workflow.jsonl"
WORKDIR="${REMUDA_MCP_WORKFLOW_DIR:-/tmp/remuda-mcp-workflow}"
HUB_PID=""
REMUDA_BIN="${REMUDA_BIN:-}"
HUB_URL=""
BOOTSTRAP=""
HOST_ID=""

log() { printf '%s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

usage() {
  cat <<'EOF' >&2
Usage: scripts/demo/mcp-workflow.sh [--skip-live]

  --skip-live   start Hub+fake Node and write mcp-config, but do not call claude
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
  if [[ -n "$HUB_PID" ]] && kill -0 "$HUB_PID" 2>/dev/null; then
    kill "$HUB_PID" 2>/dev/null || true
    wait "$HUB_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

redact_stream() {
  python3 - "$1" "$2" "$BOOTSTRAP" "$HUB_URL" "$WORKDIR" "$HOME" <<'PY'
import json, os, re, sys
src, dest, bootstrap, hub, workdir, home = sys.argv[1:7]
secrets = [s for s in (bootstrap, os.environ.get("ANTHROPIC_API_KEY", ""), os.environ.get("REMUDA_TOKEN", "")) if s]
secrets.sort(key=len, reverse=True)

def scrub(obj):
    if isinstance(obj, str):
        out = obj
        for s in secrets:
            if s:
                out = out.replace(s, "<redacted>")
        if home:
            out = out.replace(home, "<home>")
        if workdir:
            out = out.replace(workdir, "<workdir>")
        out = re.sub(r"sk-[A-Za-z0-9_-]{8,}", "sk-<redacted>", out)
        out = re.sub(r"agk_[A-Za-z0-9]+", "agk_<redacted>", out)
        out = re.sub(r"Bearer [A-Za-z0-9._-]+", "Bearer <redacted>", out)
        out = re.sub(r"boot-[a-f0-9]+", "boot-<redacted>", out)
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
        "source": "scripts/demo/mcp-workflow.sh",
        "redacted": True,
        "model": "haiku",
        "maxBudgetUsd": 0.3,
        "hub": "http://127.0.0.1:<ephemeral>",
    }
}
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

start_hub() {
  log "==> in-process Hub + fake Node (labels region=sg)"
  rm -rf "$WORKDIR/data"
  mkdir -p "$WORKDIR/data" "$WORKDIR/cwd"
  chmod 0700 "$WORKDIR"
  : >"$WORKDIR/hub.stdout"
  local logf="$WORKDIR/hub.log"
  local ready=""
  local demo_hub="$ROOT/target/debug/examples/mcp_demo_hub"
  [[ -x "$demo_hub" ]] || die "missing $demo_hub"
  REMUDA_DATA_DIR="$WORKDIR/data" "$demo_hub" \
    >"$WORKDIR/hub.stdout" 2>"$logf" &
  HUB_PID=$!
  local i
  for i in $(seq 1 80); do
    if ! kill -0 "$HUB_PID" 2>/dev/null; then
      die "mcp_demo_hub exited"$'\n'"$(tail -n 40 "$logf")"
    fi
    if ready="$(grep -m1 '^READY ' "$WORKDIR/hub.stdout" 2>/dev/null || true)" && [[ -n "$ready" ]]; then
      break
    fi
    sleep 0.2
  done
  [[ -n "$ready" ]] || die "timed out waiting for READY"$'\n'"$(tail -n 40 "$logf")"
  local payload="${ready#READY }"
  HUB_URL="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["hub"])' "$payload")"
  BOOTSTRAP="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["bootstrapToken"])' "$payload")"
  HOST_ID="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["hostId"])' "$payload")"
  log "hub $HUB_URL host $HOST_ID"
}

write_mcp_config() {
  [[ -n "$REMUDA_BIN" ]] || REMUDA_BIN="$ROOT/target/debug/remuda"
  [[ -x "$REMUDA_BIN" ]] || die "missing remuda binary at $REMUDA_BIN (cargo build -p remuda)"
  python3 - "$WORKDIR/mcp-config.json" "$REMUDA_BIN" "$HUB_URL" "$BOOTSTRAP" <<'PY'
import json, sys
path, command, hub, token = sys.argv[1:5]
cfg = {
    "mcpServers": {
        "remuda": {
            "command": command,
            "args": ["mcp"],
            "env": {
                "REMUDA_HUB": hub,
                "REMUDA_BOOTSTRAP_TOKEN": token,
                "RUST_LOG": "warn",
            },
        }
    }
}
with open(path, "w", encoding="utf-8") as fh:
    json.dump(cfg, fh, indent=2)
    fh.write("\n")
print(path)
PY
}

run_claude() {
  local attempt="$1"
  local raw="$WORKDIR/claude-$attempt.jsonl"
  local cfg="$WORKDIR/mcp-config.json"
  local prompt
  prompt="$(cat <<'PROMPT'
You are the Remuda main agent. Use only Remuda MCP tools. Do not use Bash, do not compile, do not read files.

1. Call remuda_instance_create with labels ["region=sg"], kind "claude", driver "claude-print", and prompt "say OK".
2. Parse instanceId from the tool result JSON (field instance.instanceId).
3. Call remuda_instance_wait with that instanceId, condition "run-terminal", timeoutMs 8000.
4. Call remuda_instance_read with that instanceId and limit 20.
5. Reply with one JSON object: {"instanceId":"...","waitReason":"...","readOk":true}.
PROMPT
)"
  local -a cmd=(
    claude -p "$prompt"
    --model haiku
    --max-budget-usd 0.3
    --output-format stream-json
    --verbose
    --mcp-config "$cfg"
    --strict-mcp-config
    --permission-mode dontAsk
    --no-session-persistence
  )
  if [[ "$attempt" == "1" ]]; then
    cmd+=(
      --allowedTools
      mcp__remuda__remuda_instance_create,mcp__remuda__remuda_instance_wait,mcp__remuda__remuda_instance_read
    )
  fi
  log "==> claude attempt $attempt: ${cmd[*]}"
  mkdir -p "$WORKDIR/cwd" "$WORKDIR/claude-config"
  chmod 0700 "$WORKDIR/claude-config"
  local status=0
  (
    cd "$WORKDIR/cwd"
    env -u CLAUDE_CODE_SIMPLE -u CLAUDE_CONFIG_DIR \
      "${cmd[@]}"
  ) >"$raw" 2>"$WORKDIR/claude-$attempt.err" || status=$?
  log "claude attempt $attempt exit $status"
  printf '%s\n' "$status"
}

main() {
  command -v cargo >/dev/null || die "cargo is required"
  command -v python3 >/dev/null || die "python3 is required"
  log "==> build remuda (mcp) + mcp_demo_hub example"
  local build_ok=0
  local attempt
  for attempt in 1 2; do
    if (cd "$ROOT" && cargo build -p remuda --example mcp_demo_hub) >&2; then
      build_ok=1
      break
    fi
    log "cargo build attempt $attempt failed; retrying"
    sleep 2
  done
  [[ "$build_ok" -eq 1 ]] || die "cargo build -p remuda --example mcp_demo_hub failed"
  REMUDA_BIN="$ROOT/target/debug/remuda"
  if [[ ! -x "$REMUDA_BIN" ]]; then
    (cd "$ROOT" && cargo build -p remuda) >&2 || die "cargo build -p remuda failed (needed for remuda mcp)"
  fi
  start_hub
  write_mcp_config
  log "mcp-config $WORKDIR/mcp-config.json"

  if [[ "$SKIP_LIVE" -eq 1 ]]; then
    log "skip live claude; hub still running until exit"
    return 0
  fi
  command -v claude >/dev/null || die "claude is required on PATH for the live MCP demo"

  local st1 st2
  st1="$(run_claude 1)"
  if python3 - "$WORKDIR/claude-1.jsonl" <<'PY'
import json,sys
path=sys.argv[1]
ok=False
try:
    for line in open(path, encoding="utf-8", errors="replace"):
        line=line.strip()
        if not line: continue
        try: obj=json.loads(line)
        except json.JSONDecodeError: continue
        if obj.get("type")!="assistant":
            continue
        for c in (obj.get("message") or {}).get("content") or []:
            if isinstance(c, dict) and c.get("type")=="tool_use" and "remuda_instance" in str(c.get("name","")):
                ok=True
except FileNotFoundError:
    pass
sys.exit(0 if ok else 1)
PY
  then
    redact_stream "$WORKDIR/claude-1.jsonl" "$EVIDENCE"
  else
    log "attempt 1 did not call remuda_instance_*; retrying without --allowedTools"
    st2="$(run_claude 2)"
    redact_stream "$WORKDIR/claude-2.jsonl" "$EVIDENCE"
    if [[ "$st2" != "0" && "$st1" != "0" ]]; then
      log "warning: both claude attempts exited non-zero (1=$st1 2=$st2); evidence still written"
    fi
  fi
  log "evidence $EVIDENCE"
}

main "$@"
